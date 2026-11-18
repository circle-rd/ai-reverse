//! Process-wide shutdown coordination.

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

/// A shutdown flag that flips to `true` exactly once.
#[must_use]
pub fn channel() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}

/// Resolves once shutdown was requested or every sender is gone.
pub async fn triggered(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Resolves on SIGINT or SIGTERM, or immediately when handlers cannot be installed.
pub async fn wait_for_signal() {
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn triggered_resolves_after_send() {
        let (tx, mut rx) = channel();
        let waiter = tokio::spawn(async move { triggered(&mut rx).await });
        tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn triggered_resolves_when_sender_drops() {
        let (tx, mut rx) = channel();
        drop(tx);
        tokio::time::timeout(std::time::Duration::from_secs(2), triggered(&mut rx))
            .await
            .unwrap();
    }
}
