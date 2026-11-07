//! A persistent BlueZ pairing/authorization agent.
//!
//! Without a registered agent, BlueZ rejects a speaker that reconnects *by itself*
//! ("Authentication attempt without agent … a2dp auth_cb() Access denied"). We keep an
//! interactive `bluetoothctl` running with a NoInputNoOutput ("just works") default agent
//! and answer its yes/no prompts. The adapter stays non-pairable except while the UI is
//! pairing, so only already-paired speakers ever reach these prompts.

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

/// Run the agent forever, restarting it if `bluetoothctl` exits (e.g. bluetoothd restarted).
pub async fn run() {
    loop {
        match spawn().await {
            Ok(mut child) => {
                let _ = child.wait().await;
                tracing::warn!("Bluetooth agent exited; restarting");
            }
            Err(e) => tracing::warn!("cannot start Bluetooth agent: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn spawn() -> anyhow::Result<Child> {
    let mut child = Command::new("bluetoothctl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    send(&mut stdin, "agent NoInputNoOutput").await?;

    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut buf = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = super::ctl::strip_ansi(&line);
            buf.push_str(&line);
            if line.contains("Agent registered") {
                // Only valid once registration has completed.
                let _ = send(&mut stdin, "default-agent").await;
                tracing::info!("Bluetooth agent registered");
            }
            if line.contains("(yes/no)") {
                // Authorize service / confirm just-works pairing for (already paired) speakers.
                tracing::info!("Bluetooth agent approved: {}", line.trim());
                let _ = send(&mut stdin, "yes").await;
            }
            buf.clear();
        }
    });
    Ok(child)
}

async fn send(stdin: &mut ChildStdin, cmd: &str) -> std::io::Result<()> {
    stdin.write_all(format!("{cmd}\n").as_bytes()).await?;
    stdin.flush().await
}
