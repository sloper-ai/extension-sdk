use sloper_extension::Schema;

#[derive(Schema)]
struct Nested { value: Option<Option<String>> }

#[derive(Schema)]
struct Wide { value: u64 }

#[derive(Schema)]
enum Data { Value(String) }

#[derive(Schema)]
struct Recursive { next: Vec<Recursive> }

fn main() {}
