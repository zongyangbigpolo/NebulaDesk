//! Parent-owned control pipes. The ticket is only read from stdin.
use nebula_desktop_protocol::{Event, Launch, SessionState};

pub fn run() -> anyhow::Result<()> {
    let launch = (|| {
        let launch: Launch = nebula_desktop_protocol::read_message(&mut std::io::stdin().lock())?
            .ok_or_else(|| anyhow::anyhow!("desktop launch missing"))?;
        launch.validate()?;
        let ticket = launch.ticket.try_into()?;
        anyhow::Ok((ticket, launch.resource_name))
    })();
    match launch {
        Ok((ticket, name)) => crate::session::run_managed(ticket, &name)
            .map_err(|_| anyhow::anyhow!("native session failed")),
        Err(_) => {
            // No parser error is reflected: it may quote bearer data.
            nebula_desktop_protocol::write_message(
                &mut std::io::stdout().lock(),
                &Event::State {
                    state: SessionState::Failed,
                    path: None,
                    error: Some("Native session could not start".into()),
                },
            )?;
            anyhow::bail!("native session could not start");
        }
    }
}

/// Slow or broken supervision must not stall the window/network thread.
pub(crate) struct Events {
    sender: std::sync::mpsc::SyncSender<Event>,
    writer: Option<std::thread::JoinHandle<std::io::Result<()>>>,
}

impl Events {
    pub fn start() -> std::io::Result<Self> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(256);
        let writer = std::thread::Builder::new()
            .name("nebula-desktop-events".into())
            .spawn(move || {
                let mut stdout = std::io::stdout().lock();
                for event in receiver {
                    nebula_desktop_protocol::write_message(&mut stdout, &event)?;
                }
                Ok(())
            })?;
        Ok(Self {
            sender,
            writer: Some(writer),
        })
    }

    pub fn emit(&self, event: Event) -> anyhow::Result<()> {
        self.sender
            .try_send(event)
            .map_err(|_| anyhow::anyhow!("desktop event pipe is unavailable"))
    }

    pub fn finish(mut self) -> anyhow::Result<()> {
        drop(self.sender);
        // A parent which stops consuming stdout is not allowed to pin shutdown.
        let writer = self.writer.take().expect("writer owned");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !writer.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !writer.is_finished() {
            anyhow::bail!("desktop event pipe stalled");
        }
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("desktop event writer failed"))??;
        Ok(())
    }
}
