use rand::Rng;
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
    time::Duration,
};

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

/// Parse player file.
fn parse_players(path: &Path) -> Result<HashSet<String>, Box<Error>> {
    let file = BufReader::new(File::open(path)?);
    let mut players = HashSet::new();
    'read_lines: for (i, line) in file.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue 'read_lines;
        }
        for c in line.chars() {
            if !(c.is_alphanumeric() || c.is_whitespace()) {
                eprintln!("invalid player in line {}", i + 1);
                continue 'read_lines;
            }
        }
        //Add this player
        players.insert(line.to_string());
    }
    eprintln!(
        "{} deadly players: {}",
        players.len(),
        players
            .iter()
            .map(|s| &**s)
            .collect::<Vec<&str>>()
            .join(", ")
    );
    Ok(players)
}

/// "Parse" lang file.
fn parse_lang(path: &Path) -> Result<Vec<String>, Box<Error>> {
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
                        //Only include alphanumeric/whitespace characters
                        !(c.is_alphanumeric() || c.is_whitespace())
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

fn start_server(path: &Path) -> Result<(Child, Sender<String>, Receiver<String>), Box<Error>> {
    //Start server
    eprintln!("starting server jar on \"{}\"", path.display());
    let mut server = Command::new("java")
        .arg("-jar")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    //Start threads that accumulate output on the `out` channel
    let output = {
        let (out_tx, out_rx) = mpsc::channel::<String>();
        read_pipe(server.stdout.take().unwrap(), &out_tx);
        read_pipe(server.stderr.take().unwrap(), &out_tx);
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

fn on_death<'a>(username: &'a str, input: &Sender<String>) -> Result<Option<&'a str>, Box<Error>> {
    eprintln!("player {} died, rolling dice", username);
    let cmd = |msg: String| {
        input.send(msg).unwrap();
    };
    let sleep = |time: f32| {
        thread::sleep(Duration::from_millis((time * 1000.0) as u64));
    };
    cmd(format!("say {} died", username));
    sleep(3.0);
    cmd(format!("say Rolling dice..."));
    sleep(6.0);
    let num = rand::thread_rng().gen_range(1, 21);
    cmd(format!("say Rolled {}", num));
    sleep(2.0);
    let death = [1, 4, 7, 9, 13].iter().any(|&n| n == num);
    if death {
        cmd(format!("say Always lucky boii"));
        eprintln!("rolled deadly number");
        Ok(Some(username))
    } else {
        cmd(format!("say Safe"));
        eprintln!("rolled safe number");
        Ok(None)
    }
}

/// Boolean indicates whether to continue running.
fn run_server(
    server_path: &Path,
    lang_path: &Path,
    world_path: &Path,
    players_path: &Path,
) -> Result<bool, Box<Error>> {
    //Parse configuration
    let players = parse_players(players_path)?;
    let death_msg = parse_lang(lang_path)?;
    //Start server
    let (mut server, input, output) = start_server(server_path)?;
    //Parse output to detect deaths
    let mut reset = false;
    'read_line: for line in output.iter() {
        //Clean the message of prefixes
        let line = {
            let mut line = &line[..];
            //Strip the first few `[...]`
            for _ in 0..3 {
                match line.find(']') {
                    Some(bracket) => line = &line[bracket + 1..],
                    None => continue 'read_line,
                };
            }
            //Advance until an alphanumeric character is reached
            match line.find(|c: char| c.is_alphanumeric()) {
                Some(line_start) => &line[line_start..],
                None => continue 'read_line,
            }
        };
        //Player name is the first word
        let msg_start = line
            .find(|c: char| !c.is_alphanumeric())
            .unwrap_or(line.len());
        let (username, msg) = line.split_at(msg_start);
        let username = match players.get(username) {
            Some(un) => &*un,
            None => continue 'read_line,
        };
        //Compare with death messages
        if death_msg.iter().any(|dm| msg.starts_with(dm)) {
            //Player died
            if let Some(_username) = on_death(username, &input)? {
                reset = true;
                break;
            }
        }
        //Stop if server stopped
        if server.try_wait()?.is_some() {
            break;
        }
    }
    if reset {
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
        //Continue running
        Ok(true)
    } else {
        //Stop running
        Ok(false)
    }
}

fn run() -> Result<(), Box<Error>> {
    //Parse args
    let mut args = env::args_os().skip(1);
    let server = PathBuf::from(args.next().ok_or("no server path")?);
    if server.extension() != Some("jar".as_ref()) {
        return Err("server must be a .jar file".into());
    }
    let lang = PathBuf::from(args.next().ok_or("no lang path")?);
    let world = PathBuf::from(args.next().ok_or("no world path")?);
    if world.exists() && !fs::metadata(&world)?.is_dir() {
        return Err("world must be a directory".into());
    }
    let players = PathBuf::from(args.next().ok_or("no players path")?);
    if players.extension() != Some("txt".as_ref()) {
        return Err("players must be a .txt file".into());
    }
    //Run server
    while run_server(
        server.as_ref(),
        lang.as_ref(),
        world.as_ref(),
        players.as_ref(),
    )? {
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
            eprintln!("usage: trust_hardcore <server> <lang> <world> <players>");
        }
    }
}
