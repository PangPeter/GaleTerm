//! Spawn a local PTY and pump bytes into a [`gale_term_view::TerminalPane`].

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::thread;

use async_channel::Receiver;
use gale_term::{LocalPty, PtySize};
use gale_term_view::{TerminalPane, TerminalPaneEvent, TerminalResized};
use gpui_kit::*;

pub struct LocalTerminalTab {
    pub pane: Entity<TerminalPane>,
    pty: Arc<Mutex<LocalPty>>,
}

impl LocalTerminalTab {
    pub fn spawn(window: &mut Window, cx: &mut App) -> anyhow::Result<Self> {
        let cols = 100u16;
        let rows = 30u16;
        let pty = LocalPty::spawn_shell(PtySize { cols, rows })?;
        let mut reader = pty.try_clone_reader()?;
        let pty = Arc::new(Mutex::new(pty));

        let pane = cx.new(|_cx| TerminalPane::new(cols as usize, rows as usize));
        let pane_for_task = pane.downgrade();
        let pty_for_events = pty.clone();

        let pty_for_input = pty.clone();
        cx.subscribe(&pane, move |_, ev: &TerminalPaneEvent, _| {
            if let TerminalPaneEvent::Output(data) = ev {
                if let Ok(pty) = pty_for_input.lock() {
                    let _ = pty.write_all(data);
                }
            }
        })
        .detach();

        let pty_for_resize = pty.clone();
        cx.subscribe(&pane, move |_, ev: &TerminalResized, _| {
            let size = PtySize {
                cols: ev.cols.min(u16::MAX as usize) as u16,
                rows: ev.rows.min(u16::MAX as usize) as u16,
            };
            if let Ok(pty) = pty_for_resize.lock() {
                let _ = pty.resize(size);
            }
        })
        .detach();

        let (tx, rx): (_, Receiver<Vec<u8>>) = async_channel::unbounded();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send_blocking(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        window
            .spawn(cx, async move |cx| {
                while let Ok(chunk) = rx.recv().await {
                    let pty_for_events = pty_for_events.clone();
                    if pane_for_task
                        .update(cx, |pane, cx| {
                            let events = pane.feed_bytes(&chunk, cx);
                            for ev in events {
                                if let TerminalPaneEvent::Output(data) = ev {
                                    if let Ok(pty) = pty_for_events.lock() {
                                        let _ = pty.write_all(&data);
                                    }
                                }
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();

        Ok(Self { pane, pty })
    }

    pub fn write_input(&self, data: &[u8]) {
        if let Ok(pty) = self.pty.lock() {
            let _ = pty.write_all(data);
        }
    }
}
