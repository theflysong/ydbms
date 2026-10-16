#[test]
fn tmp_ast() {
    let parsed = pg_query::parse(
        "SELECT k FROM big AS b WHERE EXISTS (SELECT 1 FROM t WHERE x = b.k)",
    )
    .unwrap();
    let nodes = parsed.protobuf.nodes();
    for n in nodes {
        println!("{n:?}");
    }
}
