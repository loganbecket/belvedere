//! The Belvedere panel applet. For now it only reports its version.

fn main() {
    println!("{}", belvedere_core::version_line("belvedere-applet"));
}
