use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("fatal: cannot determine current directory: {e}");
            return ExitCode::from(128);
        }
    };
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();
    let (mut input, mut out, mut err) = (stdin.lock(), io::BufWriter::new(stdout.lock()), stderr.lock());
    let mut ctx = nanogit::cli::Ctx { cwd: &cwd, out: &mut out, err: &mut err, input: &mut input };
    let code = match nanogit::cli::run(&args, &mut ctx) {
        Ok(code) => code,
        Err(e) => {
            let _ = ctx.out.flush();
            let _ = writeln!(ctx.err, "fatal: {e}");
            128
        }
    };
    let _ = out.flush();
    ExitCode::from(code as u8)
}
