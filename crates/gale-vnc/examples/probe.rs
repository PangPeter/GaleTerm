use gale_vnc::{VncClientConfig, VncInput, VncOutput, VncSession};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = VncClientConfig::new("127.0.0.1", 5901);
    config.password = Some("secret".into());
    let session = VncSession::connect(config).await?;
    println!("connected: {} {}x{}", session.name(), session.size().0, session.size().1);

    let (tx_in, rx_in) = mpsc::unbounded_channel();
    let (tx_out, mut rx_out) = mpsc::unbounded_channel();
    let runner = tokio::spawn(async move { session.run(rx_in, tx_out).await });

    let mut frames = 0u32;
    while let Some(msg) = rx_out.recv().await {
        match msg {
            VncOutput::Ready { width, height, name } => {
                println!("ready {name} {width}x{height}");
            }
            VncOutput::Frame(fb) => {
                frames += 1;
                let non_black = fb.pixels.chunks(4).filter(|p| p[..3] != [0, 0, 0]).count();
                println!(
                    "frame {frames}: {}x{} non_black={non_black}/{}",
                    fb.width,
                    fb.height,
                    fb.pixels.len() / 4
                );
                if frames >= 2 {
                    let _ = tx_in.send(VncInput::Close);
                    break;
                }
            }
            VncOutput::Bell => println!("bell"),
            VncOutput::CutText(t) => println!("cut: {t:?}"),
        }
    }
    println!("runner: {:?}", runner.await?);
    Ok(())
}
