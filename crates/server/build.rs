// #[sqlx::test(migrations = "../../migrations")] embeds the SQL files at compile time.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
