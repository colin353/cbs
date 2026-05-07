#[tokio::main]
async fn main() {
    let (mut tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.send(41_u8).await.unwrap();
    assert_eq!(rx.recv().await, Some(41));
}
