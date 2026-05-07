#[derive(serde_derive::Serialize)]
struct Message {
    id: u64,
    text: &'static str,
}

fn assert_serialize<T: serde::Serialize>(_value: &T) {}

fn main() {
    let message = Message {
        id: 7,
        text: "hello",
    };
    assert_serialize(&message);
}
