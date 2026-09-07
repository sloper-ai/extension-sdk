use sloper_extension::Schema;
use std::collections::BTreeMap;

#[derive(Schema)]
struct Wrapped<T, const N: usize> {
    list: Vec<T>,
    fixed: [T; N],
    map: BTreeMap<String, T>,
    nullable: Option<T>,
}

fn main() {
    assert_eq!(Wrapped::<String, 3>::TYPE, "object");
    assert!(Wrapped::<String, 3>::JSON.contains("\"maxItems\":3"));
}
