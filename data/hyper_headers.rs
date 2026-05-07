fn main() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain"),
    );
    assert_eq!(headers.len(), 1);
}
