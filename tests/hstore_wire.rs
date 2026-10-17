//! hstore's binary form on the wire, which a driver with an hstore adapter sends and asks for: a count, then
//! each key and value with its length before it.

use chronos::sql::{hstore_from_wire, hstore_to_wire};

#[test]
fn hstore_binary_form_round_trips() {
    let text = r#""a"=>"1", "b"=>NULL"#;
    let b = hstore_to_wire(text).unwrap();
    assert_eq!(b, [0, 0, 0, 2, 0, 0, 0, 1, b'a', 0, 0, 0, 1, b'1', 0, 0, 0, 1, b'b', 255, 255, 255, 255]);
    assert_eq!(hstore_from_wire(&b).unwrap(), text);
    assert_eq!(hstore_from_wire(&hstore_to_wire("").unwrap()).unwrap(), "");
    // (keys come back in hstore's order, each once)
    let unsorted = [0, 0, 0, 2, 0, 0, 0, 2, b'z', b'z', 0, 0, 0, 0, 0, 0, 0, 1, b'k', 0, 0, 0, 1, b'v'];
    assert_eq!(hstore_from_wire(&unsorted).unwrap(), r#""k"=>"v", "zz"=>"""#);
    // what isn't one: cut short, a length past the end, its text
    assert!(hstore_from_wire(&b[..b.len() - 1]).is_none());
    assert!(hstore_from_wire(&[0, 0, 0, 1, 0, 0, 0, 9, b'a']).is_none());
    assert!(hstore_from_wire(b"a=>1").is_none());
}
