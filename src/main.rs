fn main() -> rustsql::common::error::Result<()> {
    rustsql::repl::run_memory_repl()
}

#[cfg(test)]
mod tests {
    #[test]
    fn main_has_expected_entry_signature() {
        let entry: fn() -> rustsql::common::error::Result<()> = super::main;
        let _ = entry;
    }
}
