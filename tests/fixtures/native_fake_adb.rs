//! Dependency-free executable fixture for Windows CreateProcess and Unix pipes.
//! Build with rustc; it never invokes a shell, SDK or another ADB executable.
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

const PREFIX: &[u8] = b"10-23 12:00:00.000 1234 1235 I NativeAdb: ";

fn trace(event: &str) {
    if let Some(directory) = env::var_os("CIALLOGCAT_NATIVE_TRACE") {
        let directory = PathBuf::from(directory);
        let _ = fs::create_dir_all(&directory);
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(format!("{}.trace", std::process::id())))
        {
            let _ = writeln!(file, "{event}");
        }
    }
}

fn write(bytes: &[u8]) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()
}
fn line(message: &[u8]) -> io::Result<()> {
    let mut bytes = PREFIX.to_vec();
    bytes.extend_from_slice(message);
    bytes.push(b'\n');
    write(&bytes)
}
fn wait_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
fn buffers(args: &[String]) -> bool {
    !args.is_empty()
        && args.len().is_multiple_of(2)
        && args.chunks_exact(2).all(|pair| {
            pair[0] == "-b"
                && matches!(
                    pair[1].as_str(),
                    "main" | "system" | "crash" | "events" | "radio" | "all"
                )
        })
}
fn scoped_buffers(args: &[String]) -> Option<bool> {
    let (dump, offset) = match args.first()?.as_str() {
        "-d" => (true, 1),
        "-t" | "-T" => {
            let value = args.get(1)?;
            if value.is_empty()
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b"- :.".contains(&b))
                || value.starts_with('-')
            {
                return None;
            }
            (args[0] == "-t", 2)
        }
        _ => return None,
    };
    buffers(&args[offset..]).then_some(dump)
}
// Only single-quoted literals are needed by this protocol fixture. It never
// executes a shell and rejects an unfinished quote or unexpected buffer words.
fn remote_words(value: &str) -> Option<Vec<String>> {
    let mut quoted = false;
    let mut current = String::new();
    let mut words = Vec::new();
    for ch in value.chars() {
        match ch {
            '\'' => quoted = !quoted,
            ' ' if !quoted => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    (!quoted).then_some(words)
}
fn payload() -> Vec<u8> {
    // Independent AOSP field encoding, including NUL, LF and invalid UTF-8.
    b"\x08\x01\x10\x02\x18\x08\x20\x2a\x28\x04\x30\x05\x3a\x04Tag\x00\x42\x04m\x00\xff\n\x4a\x03app"
        .to_vec()
}
fn proto(scenario: &str, dump: bool) -> io::Result<i32> {
    if scenario == "proto_unsupported" {
        eprintln!("native fake adb: unknown option --proto");
        return Ok(1);
    }
    let payload = payload();
    let length = if scenario == "proto_truncated" {
        payload.len() + 5
    } else {
        payload.len()
    };
    // Deliberately fragmented binary writes exercise exact frame assembly.
    let frame = [(length as u64).to_le_bytes().as_slice(), &payload].concat();
    for chunk in frame.chunks(3) {
        write(chunk)?;
    }
    trace("stream_ready");
    if scenario == "proto_normal" && !dump {
        wait_forever();
    }
    Ok(0)
}
fn execute(args: &[String], scenario: &str) -> io::Result<i32> {
    if args == ["devices", "-l"] {
        write(b"List of devices attached\nTEST-USB\tdevice model:Native_Test\n\n")?;
        return Ok(0);
    }
    if args.len() < 3 || args[0] != "-s" || args[1] != "TEST-USB" {
        return Ok(2);
    }
    let rest = &args[2..];
    if rest == ["features"] {
        write(if scenario == "proto_no_shell_v2" {
            b"cmd,stat_v2\n"
        } else {
            b"shell_v2,cmd,stat_v2\n"
        })?;
        return Ok(0);
    }
    if rest == ["shell", "pm", "list", "packages", "-3"] {
        write(b"package:com.example.native\n")?;
        return Ok(0);
    }
    if rest == ["shell", "ps", "-A", "-o", "PID,NAME"] {
        write(b"PID NAME\n1234 com.example.native\n")?;
        return Ok(0);
    }
    if rest == ["shell", "ps", "-A"] {
        write(b"USER PID NAME\nu0_a1 1234 com.example.native\n")?;
        return Ok(0);
    }
    if rest.len() == 4 && rest[..3] == ["shell", "-T", "-n"] {
        let Some(remote) = rest[3]
            .strip_prefix("exec logcat --proto -v threadtime ")
            .and_then(|v| v.strip_suffix(" '*:V'"))
        else {
            return Ok(2);
        };
        let Some(dump) = remote_words(remote).and_then(|words| scoped_buffers(&words)) else {
            return Ok(2);
        };
        return proto(scenario, dump);
    }
    if rest.len() < 6
        || rest[..3] != ["logcat", "-v", "threadtime"]
        || scoped_buffers(&rest[3..]).is_none()
    {
        return Ok(2);
    }
    match scenario {
        "snapshot" => {
            line(b"stored snapshot")?;
            trace("stream_ready");
            if scoped_buffers(&rest[3..]) == Some(false) {
                wait_forever();
            }
        }
        "once_then_idle" => {
            line(b"before idle")?;
            trace("stream_ready");
            wait_forever();
        }
        "continuous" => {
            trace("stream_ready");
            for index in 0..10000 {
                line(format!("continuous-{index}").as_bytes())?;
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        "stream_edges" => {
            write(b"--------- beginning of main\n")?;
            line("unicode 雪 🚀".as_bytes())?;
            line(b"invalid \xff byte")?;
            let mut final_line = PREFIX.to_vec();
            final_line.extend_from_slice(b"final partial");
            write(&final_line)?;
        }
        "stderr_exit" => {
            eprintln!("native fake adb: permission denied");
            return Ok(17);
        }
        "burst" => {
            trace("stream_ready");
            let mut bytes = b"--------- burst ".to_vec();
            bytes.resize(256 * 1024 - 1, b'x');
            bytes.push(b'\n');
            for _ in 0..128 {
                write(&bytes)?;
            }
            trace("burst_complete");
            wait_forever();
        }
        _ => return Ok(2),
    }
    Ok(0)
}
fn main() {
    // A blocked fixture can never survive indefinitely after a failed test.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(12));
        std::process::exit(124);
    });
    let args: Vec<_> = env::args().skip(1).collect();
    let scenario =
        env::var("CIALLOGCAT_NATIVE_SCENARIO").unwrap_or_else(|_| "once_then_idle".into());
    trace("invocation");
    let code = match execute(&args, &scenario) {
        Ok(code) => code,
        Err(_) => 19,
    };
    trace("exit");
    std::process::exit(code);
}
