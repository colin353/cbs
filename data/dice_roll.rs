fn main() {
    let r: u32 = rand::random();
    println!("you rolled: {:#?}", (r % 6) + 1);
}
