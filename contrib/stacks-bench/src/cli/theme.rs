pub struct CliTheme;

impl cliclack::Theme for CliTheme {
    fn progress_chars(&self) -> String {
        "⣿⣾⣽⣻⢿⡿⣟⣯⣷⣀".to_string()
    }

    // fn default_progress_template(&self) -> String {
    //     "{spinner:.cyan} {msg:20} {percent:>3}% |{bar:30.cyan/blue}| {pos:>7}/{len:7} • {per_sec:>10} • ETA {eta_precise}".to_string()
    // }
}
