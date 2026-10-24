// sqlx::migrate! and #[sqlx::test(migrations = ...)] embed the SQL files at compile time.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
