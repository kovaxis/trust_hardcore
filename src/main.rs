use rand::Rng;
use serde_derive::Deserialize;
use serde_json as json;
use std::{
    collections::HashSet,
    env,
    error::Error,
    fs::{self, File},
    io::{self, prelude::*, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct Config {
    server: Vec<String>,
    world: PathBuf,
    lang: PathBuf,
    ignore_phrases: Vec<String>,
    make_backups: bool,
    backup_dir: PathBuf,
    players: Vec<String>,
    allow_all_players: bool,
    on_death_command: Option<String>,
    checkpoint_minutes: u64,
    roll_range: (i32, i32),
    deadly_rolls: Vec<i32>,
    bracket_count: u32,
}

const USERNAME_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_-0123456789";
fn is_username_char(c: char) -> bool {
    let mut is_username = [false; 128];
    for &c in USERNAME_CHARS.as_bytes().iter() {
        is_username[c as usize] = true;
    }
    (c as u32) < 128 && is_username[c as usize]
}

enum Penalty {
    None,
    Rewind,
    Reset,
}

fn bytes_to_string(mut bytes: &[u8]) -> String {
    while bytes
        .first()
        .map(|ch| ch.is_ascii_whitespace())
        .unwrap_or(false)
    {
        bytes = &bytes[1..];
    }
    while bytes
        .last()
        .map(|ch| ch.is_ascii_whitespace())
        .unwrap_or(false)
    {
        bytes = &bytes[..bytes.len() - 1];
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn read_pipe<R: Read + Send + 'static>(pipe: R, sendback: &Sender<String>) {
    let sendback = sendback.clone();
    thread::spawn(move || {
        let buf = BufReader::new(pipe);
        for line in buf.split(b'\n') {
            let line = bytes_to_string(&line.unwrap());
            println!("{}", line);
            if let Err(_line) = sendback.send(line.to_string()) {
                //Channel closed
                break;
            }
        }
    });
}

fn load_config(path: &Path) -> Result<Config, Box<dyn Error>> {
    macro_rules! ensure {
        ($cond:expr, $($tt:tt)*) => {{
            if !$cond {
                return Err(format!($($tt)*).into());
            }
        }};
    }
    let conf: Config = json::from_reader(File::open(path)?)?;
    /*ensure!(
        conf.server.extension() == Some("jar".as_ref()),
        "server must be a .jar file"
    );*/
    ensure!(
        !conf.world.exists() || fs::metadata(&conf.world)?.is_dir(),
        "world must be a directory"
    );
    ensure!(
        conf.backup_dir.exists() && fs::metadata(&conf.backup_dir)?.is_dir(),
        "backup must be a directory"
    );
    ensure!(
        conf.roll_range.0 <= conf.roll_range.1,
        "start of roll range must be smaller than its end"
    );
    for &num in &conf.deadly_rolls {
        if num < conf.roll_range.0 || num > conf.roll_range.1 {
            eprintln!(
                "warning: deadly roll {} is outside of roll range [{}, {}]",
                num, conf.roll_range.0, conf.roll_range.1
            );
        }
    }
    Ok(conf)
}

/// "Parse" lang file.
fn parse_lang(path: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let mut death_msg = Vec::new();
    let lang = BufReader::new(File::open(path)?);
    for line in lang.lines() {
        let line = line?;
        if line.contains("death.") {
            //Death line
            let pat = "%1$s";
            if let Some(from_idx) = line.find(pat) {
                //Up to what index to include the pattern
                let msg = &line[from_idx + pat.len()..];
                let msg_len = msg
                    .find(|c: char| {
                        //Only include alphanumeric/whitespace/apostrophe characters
                        !(c.is_alphanumeric() || c.is_whitespace() || c == '\'')
                    })
                    .unwrap_or(msg.len());
                //Insert this message
                death_msg.push(msg[..msg_len].trim_end().to_string());
            }
        }
    }
    eprintln!("{} death messages:", death_msg.len());
    for msg in death_msg.iter() {
        eprintln!("    \"{}\"", msg);
    }
    Ok(death_msg)
}

fn start_server(
    cmd: &[String],
) -> Result<(Child, Sender<String>, Receiver<String>), Box<dyn Error>> {
    //Start server
    eprintln!("starting server jar using command \"{:?}\"", cmd);
    let mut server = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    //Start threads that accumulate output on the `out` channel
    let output = {
        let (out_tx, out_rx) = mpsc::channel::<String>();
        read_pipe(server.stdout.take().unwrap(), &out_tx);
        read_pipe(server.stderr.take().unwrap(), &out_tx);
        //Send periodic empty messages
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(10));
            if let Err(_closed) = out_tx.send(String::new()) {
                break;
            }
        });
        out_rx
    };

    let input = {
        //Start thread that accumulates input and sends it to the server
        let (in_tx, in_rx) = mpsc::channel::<String>();
        {
            let mut stdin = server.stdin.take().unwrap();
            thread::spawn(move || {
                for cmd in in_rx.iter() {
                    write!(stdin, "{}\n", cmd).unwrap();
                }
            });
        }
        //Start background thread that reads program stdin
        {
            let in_tx = in_tx.clone();
            thread::spawn(move || {
                for line in io::stdin().lock().split(b'\n') {
                    let line = bytes_to_string(&line.unwrap());
                    if let Err(_line) = in_tx.send(line) {
                        //Channel closed
                        break;
                    }
                }
            });
        }
        in_tx
    };
    Ok((server, input, output))
}

fn on_death<'a>(
    config: &Config,
    username: &'a str,
    input: &Sender<String>,
) -> Result<Penalty, Box<dyn Error>> {
    eprintln!("player {} died, rolling dice", username);
    let cmd = |msg: String| {
        input.send(msg).unwrap();
    };
    if let Some(death_cmd) = config.on_death_command.as_ref() {
        cmd(death_cmd.replace("{username}", username));
    }
    let sleep = |time: f32| {
        thread::sleep(Duration::from_millis((time * 1000.0) as u64));
    };
    cmd(format!("say {} died", username));
    sleep(3.0);
    cmd(format!("say Rolling dice..."));
    sleep(6.0);
    let num = rand::thread_rng().gen_range(config.roll_range.0, config.roll_range.1 + 1);
    cmd(format!("say Rolled {}", num));
    sleep(2.0);
    let death = config.deadly_rolls.iter().any(|&n| n == num);
    if death {
        cmd(format!("say Always lucky boii"));
        sleep(1.0);
        eprintln!("rolled bad number");
        Ok(Penalty::Reset)
    } else {
        eprintln!("rolled good number");
        Ok(Penalty::None)
    }
}

fn save_playtime(world_path: &Path, playtime: Duration) -> Result<(), Box<dyn Error>> {
    let path = world_path.join("playtime.txt");
    let mut file = File::create(&path)?;
    write!(file, "{}", playtime.as_secs())?;
    Ok(())
}

fn load_playtime(world_path: &Path) -> Result<Duration, Box<dyn Error>> {
    let path = world_path.join("playtime.txt");
    let playtime = fs::read_to_string(&path)?;
    let playtime: u64 = playtime.parse()?;
    Ok(Duration::from_secs(playtime))
}

fn copy_dir(from: &mut PathBuf, to: &mut PathBuf) -> Result<(), Box<dyn Error>> {
    if !to.exists() {
        fs::create_dir(&*to)?;
    }
    for entry in fs::read_dir(&*from)? {
        let name = entry?.file_name();
        from.push(&name);
        to.push(&name);
        if let Ok(meta) = from.metadata() {
            if meta.is_dir() {
                copy_dir(from, to)?;
            } else if meta.is_file() {
                fs::copy(&*from, &*to)?;
            }
        }
        from.pop();
        to.pop();
    }
    Ok(())
}

fn make_backup(
    world_path: &Path,
    backup_path: &Path,
    input: &Sender<String>,
) -> Result<(), Box<dyn Error>> {
    eprintln!("making backup");
    //Remove old backup
    if backup_path.exists() {
        fs::remove_dir_all(&backup_path)?;
    }
    //Force server to backup
    input.send(format!("save-all")).unwrap();
    thread::sleep(Duration::from_secs(5));
    input.send(format!("save-off")).unwrap();
    thread::sleep(Duration::from_secs(1));
    //Copy save file
    copy_dir(
        &mut world_path.to_path_buf(),
        &mut backup_path.to_path_buf(),
    )?;
    //Re-enable saving
    input.send(format!("save-on")).unwrap();
    input.send(format!("say Checkpoint!")).unwrap();
    Ok(())
}

fn update_playtime(
    config: &Config,
    players_online_since: &mut Option<Instant>,
    playtime: &mut Duration,
) -> Result<bool, Box<dyn Error>> {
    if let Some(since) = players_online_since {
        //Advance playtime
        let now = Instant::now();
        let adv = now - *since;
        if adv > Duration::from_secs(8) {
            let old_playtime = *playtime;
            *playtime += adv;
            *since = now;
            eprintln!("advancing by {}ms", adv.as_millis());
            eprintln!("new playtime: {}ms", playtime.as_millis());
            //Save playtime
            save_playtime(&*config.world, *playtime)?;
            //Make backup if advanced past the boundary
            let backup_interval = config.checkpoint_minutes * 60;
            let backup_count =
                |playtime: Duration| (playtime.as_secs() + backup_interval - 30) / backup_interval;
            if backup_count(*playtime) > backup_count(old_playtime) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Boolean indicates whether to continue running.
fn run_server(config_path: &Path) -> Result<bool, Box<dyn Error>> {
    //Load config
    let mut config = load_config(config_path)?;
    let backup_path = config.backup_dir.join(
        config
            .world
            .file_name()
            .ok_or("no world name (invalid world path)")?,
    );
    let backup_path = &*backup_path;
    let world_path = &*config.world;
    let players = {
        let mut players = HashSet::new();
        eprintln!("{} deadly players:", config.players.len());
        for player in config.players.drain(..) {
            eprintln!("    {}", player);
            players.insert(player);
        }
        players
    };
    let death_msg = parse_lang(config.lang.as_ref())?;
    //Keep track of online players
    let mut online_players = HashSet::new();
    let mut players_online_since = None;
    let mut playtime = load_playtime(world_path).unwrap_or_else(|err| {
        eprintln!("failed to read playtime: {}", err);
        Duration::from_secs(0)
    });
    eprintln!("have played for {} seconds", playtime.as_secs());
    //Start server
    let (mut server, input, output) = start_server(&*config.server)?;
    //Parse output to detect deaths
    let mut penalty = Penalty::None;
    'read_line: for line in output.iter() {
        //Bookkeep playtime
        if update_playtime(&config, &mut players_online_since, &mut playtime)?
            && config.make_backups
        {
            make_backup(world_path, backup_path, &input)?;
        }
        //Clean the message of prefixes
        let line = {
            let mut line = &line[..];
            //Strip the first few `[...]`
            for _ in 0..config.bracket_count {
                match line.find(']') {
                    Some(bracket) => line = &line[bracket + 1..],
                    None => continue 'read_line,
                };
            }
            //Advance until a username character is reached
            match line.find(is_username_char) {
                Some(line_start) => &line[line_start..],
                None => continue 'read_line,
            }
        };
        //Player name is the first word
        let msg_start = line
            .find(|c: char| !is_username_char(c))
            .unwrap_or(line.len());
        let (username, msg) = line.split_at(msg_start);
        let username = username.to_string();
        if !config.allow_all_players && !players.contains(&username) {
            continue 'read_line;
        }
        //Compare with death messages
        if death_msg.iter().any(|dm| msg.starts_with(dm))
            && !config.ignore_phrases.iter().any(|dm| msg.starts_with(dm))
        {
            //Player died
            penalty = on_death(&config, &username, &input)?;
            match penalty {
                Penalty::Rewind | Penalty::Reset => break,
                _ => (),
            }
        } else if msg.starts_with(" joined the game") {
            if online_players.is_empty() {
                //Start counting time
                eprintln!("started counting time");
                players_online_since = Some(Instant::now());
            }
            eprintln!("{} went online", username);
            online_players.insert(username);
        } else if msg.starts_with(" left the game") {
            eprintln!("{} went offline", username);
            online_players.remove(&username);
            if online_players.is_empty() {
                //Stop counting time
                eprintln!("stopped counting time");
                players_online_since = None;
            }
        }
        //Stop if server stopped
        if server.try_wait()?.is_some() {
            break;
        }
    }
    match penalty {
        Penalty::None => {
            //Stop running
            Ok(false)
        }
        Penalty::Rewind if backup_path.exists() => {
            //Restore backup
            eprintln!("restoring backup");
            //Stop server
            input.send(format!("say Winding back...")).unwrap();
            thread::sleep(Duration::from_secs(2));
            input.send(format!("stop")).unwrap();
            //Wait for server to actually stop
            server.wait()?;
            //Delete world
            eprintln!("deleting world directory on \"{}\"", world_path.display());
            fs::remove_dir_all(&world_path)?;
            //Restore backup
            eprintln!(
                "copying backup directory \"{}\" to world directory \"{}\"",
                backup_path.display(),
                world_path.display()
            );
            copy_dir(
                &mut backup_path.to_path_buf(),
                &mut world_path.to_path_buf(),
            )?;
            //save_playtime(world_path, playtime)?;
            //Continue running
            Ok(true)
        }
        _ => {
            //Reset world
            eprintln!("resetting world");
            //Stop server
            input.send(format!("say Destroying world...")).unwrap();
            thread::sleep(Duration::from_secs(2));
            input.send(format!("stop")).unwrap();
            //Wait for server to actually stop
            server.wait()?;
            //Delete world
            eprintln!("deleting world directory on \"{}\"", world_path.display());
            fs::remove_dir_all(&world_path)?;
            //Delete backup
            if backup_path.exists() {
                eprintln!("deleting backup directory on \"{}\"", backup_path.display());
                fs::remove_dir_all(backup_path)?;
            }
            //Continue running
            Ok(true)
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    //Parse args
    let mut args = env::args_os().skip(1);
    let config = args.next().ok_or("no config path supplied")?;
    //Run server
    while run_server(config.as_ref())? {
        eprintln!();
        eprintln!();
    }
    Ok(())
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(err) => {
            eprintln!("error running program: {}", err);
            eprintln!();
            eprintln!("full error: {:?}", err);
            eprintln!();
            eprintln!("usage: trust_hardcore <config>");
        }
    }
}
