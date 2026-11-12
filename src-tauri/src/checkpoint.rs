use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, task::JoinHandle, time::Instant};

const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);

pub fn start<F>(write: F) -> (watch::Sender<u64>, JoinHandle<()>)
where
    F: Fn(u64) -> Result<u64, String> + Send + Sync + 'static,
{
    start_with_interval(CHECKPOINT_INTERVAL, write)
}

fn start_with_interval<F>(interval: Duration, write: F) -> (watch::Sender<u64>, JoinHandle<()>)
where
    F: Fn(u64) -> Result<u64, String> + Send + Sync + 'static,
{
    let (sender, mut updates) = watch::channel(0);
    let write = Arc::new(write);
    let task = tokio::spawn(async move {
        let mut saved_revision = 0;
        let mut next_attempt = Instant::now();
        while updates.has_changed().is_ok() {
            let dirty = *updates.borrow_and_update() > saved_revision;
            if !dirty {
                if updates.changed().await.is_err() {
                    break;
                }
                continue;
            }
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(next_attempt) => {}
                changed = updates.changed() => {
                    if changed.is_err() { break; }
                    continue;
                }
            }
            let revision = *updates.borrow_and_update();
            if updates.has_changed().is_err() {
                break;
            }
            let write = write.clone();
            match tokio::task::spawn_blocking(move || write(revision)).await {
                Ok(Ok(revision)) => saved_revision = revision,
                Ok(Err(error)) => eprintln!("无法保存测速快照：{error}"),
                Err(error) => eprintln!("测速快照写入任务失败：{error}"),
            }
            next_attempt = Instant::now() + interval;
        }
    });
    (sender, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    const TEST_INTERVAL: Duration = Duration::from_millis(40);

    async fn observed_revision(writes: &mut tokio::sync::mpsc::UnboundedReceiver<u64>) -> u64 {
        tokio::time::timeout(Duration::from_secs(3), writes.recv())
            .await
            .expect("checkpoint writer timed out")
            .expect("checkpoint writer exited")
    }

    #[tokio::test]
    async fn saves_newest_dirty_revision_at_deadline_without_another_update() {
        let (written, mut writes) = tokio::sync::mpsc::unbounded_channel();
        let (updates, writer) = start_with_interval(TEST_INTERVAL, move |revision| {
            written.send(revision).unwrap();
            Ok(revision)
        });
        updates.send_replace(1);
        assert_eq!(observed_revision(&mut writes).await, 1);
        updates.send_replace(2);
        updates.send_replace(3);
        assert_eq!(observed_revision(&mut writes).await, 3);
        drop(updates);
        writer.await.unwrap();
        assert!(writes.try_recv().is_err());
    }

    #[tokio::test]
    async fn retries_dirty_revision_without_another_update() {
        let (attempted, mut attempts) = tokio::sync::mpsc::unbounded_channel();
        let first = AtomicBool::new(true);
        let (updates, writer) = start_with_interval(TEST_INTERVAL, move |revision| {
            attempted.send(revision).unwrap();
            if first.swap(false, Ordering::Relaxed) {
                return Err("fixture write failure".into());
            }
            Ok(revision)
        });
        updates.send_replace(1);
        assert_eq!(observed_revision(&mut attempts).await, 1);
        assert_eq!(observed_revision(&mut attempts).await, 1);
        drop(updates);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn closes_after_in_flight_write_and_discards_unneeded_trailing_write() {
        let (written, mut writes) = tokio::sync::mpsc::unbounded_channel();
        let (release, released) = std::sync::mpsc::channel();
        let released = std::sync::Mutex::new(released);
        let (updates, writer) = start_with_interval(TEST_INTERVAL, move |revision| {
            written.send(revision).unwrap();
            released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            Ok(revision)
        });
        updates.send_replace(1);
        assert_eq!(observed_revision(&mut writes).await, 1);
        updates.send_replace(2);
        drop(updates);
        assert!(!writer.is_finished());
        release.send(()).unwrap();
        writer.await.unwrap();
        assert!(writes.try_recv().is_err());
    }
}
