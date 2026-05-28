use miette::Result;

pub fn repair() -> Result<()> {
    crate::commands::doctor::repair()
}
