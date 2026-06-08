// Copyright (c) 2026 Kata Contributors
//
// SPDX-License-Identifier: Apache-2.0
//

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent::sock::Vsock;
use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use url::Url;

const DIAL_RETRY: Duration = Duration::from_secs(2);
const VSOCK_SCHEME: &str = "vsock";

pub(crate) fn guest_cid_from_agent_url(agent_url: &str) -> Result<u32> {
    let url = Url::parse(agent_url).context("parse agent url")?;
    if url.scheme() != VSOCK_SCHEME {
        return Err(anyhow!(
            "vsock UDS forward requires {VSOCK_SCHEME} agent URL, got {agent_url:?}"
        ));
    }

    url.host_str()
        .ok_or_else(|| anyhow!("cannot parse guest CID from agent URL {agent_url:?}"))?
        .parse::<u32>()
        .with_context(|| format!("invalid guest CID in agent URL {agent_url:?}"))
}

pub(crate) struct VsockUdsForward {
    shutdown_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl VsockUdsForward {
    pub(crate) fn start(guest_cid: u32, port: u32, uds: PathBuf) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        info!(
            sl!(),
            "vsock UDS forward: started guest_cid={guest_cid} port={port} uds={}",
            uds.display()
        );
        let task = tokio::spawn(run_dial_loop(guest_cid, port, uds, shutdown_rx));

        Self { shutdown_tx, task }
    }

    pub(crate) async fn stop(self) {
        let _ = self.shutdown_tx.send(true);
        self.task.abort();
        let _ = self.task.await;
    }
}

async fn run_dial_loop(
    guest_cid: u32,
    port: u32,
    uds: PathBuf,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        if *shutdown_rx.borrow() {
            return;
        }

        let vsock = Vsock::new(guest_cid, port);
        match vsock.connect_once().await {
            Ok(mut vsock) => {
                if let Err(err) = bridge(&mut vsock, &uds, &mut shutdown_rx).await {
                    debug!(
                        sl!(),
                        "vsock UDS forward: bridge ended guest_cid={guest_cid} port={port} uds={}: {err:#}",
                        uds.display()
                    );
                }
            }
            Err(err) => {
                debug!(
                    sl!(),
                    "vsock UDS forward: guest vsock dial failed guest_cid={guest_cid} port={port}: {err:#}"
                );
            }
        }

        if sleep_or_shutdown(&mut shutdown_rx, DIAL_RETRY).await {
            return;
        }
    }
}

async fn sleep_or_shutdown(shutdown_rx: &mut watch::Receiver<bool>, dur: Duration) -> bool {
    tokio::select! {
        res = shutdown_rx.wait_for(|v| *v) => res.is_ok(),
        _ = tokio::time::sleep(dur) => false,
    }
}

async fn bridge(
    vsock: &mut UnixStream,
    uds_path: &Path,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let mut first = [0u8; 1];
    let n = match vsock.read(&mut first).await {
        Ok(0) => return Ok(()),
        Ok(n) => n,
        Err(err) => {
            debug!(
                sl!(),
                "vsock UDS forward: guest vsock closed before first byte uds={}: {err:#}",
                uds_path.display()
            );
            return Ok(());
        }
    };

    let mut uds = match UnixStream::connect(uds_path).await {
        Ok(stream) => stream,
        Err(err) => {
            warn!(
                sl!(),
                "vsock UDS forward: unix dial failed uds={}: {err:#}",
                uds_path.display()
            );
            return Ok(());
        }
    };

    uds.write_all(&first[..n]).await.with_context(|| {
        format!(
            "vsock UDS forward: failed to write first byte to unix socket uds={}",
            uds_path.display()
        )
    })?;

    let (mut v_read, mut v_write) = vsock.split();
    let (mut u_read, mut u_write) = uds.into_split();

    tokio::select! {
        _ = shutdown_rx.wait_for(|v| *v) => {}
        _ = async {
            let _ = tokio::join!(
                tokio::io::copy(&mut v_read, &mut u_write),
                tokio::io::copy(&mut u_read, &mut v_write),
            );
        } => {}
    }

    Ok(())
}
