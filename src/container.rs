use crate::descriptors;
use anyhow::{Context, Result};
use nix::unistd::execvpe;
use std::ffi::CString;

/// Replace the current process with `args[0]`, passing `args` as argv and `env` as the environment.
/// Returns only on failure.
///
/// The program receives only stdin, stdout, and stderr. Other descriptors stay open until the
/// exec succeeds, so the caller can still report a failure through them.
///
/// A program name without '/' is searched in the runtime's own PATH, not in the PATH from `env`,
/// because libc's execvpe reads PATH from the calling process.
pub fn exec(args: &[String], env: &[String]) -> Result<()> {
    let program = args.first().context("no program to execute")?;
    let c_args = to_cstrings(args).context("argument contains a nul byte")?;
    let c_env = to_cstrings(env).context("environment variable contains a nul byte")?;
    descriptors::close_others_on_exec().context("keep descriptors from the program")?;

    let Err(error) = execvpe(&c_args[0], &c_args, &c_env);
    Err(error).with_context(|| format!("exec {program:?}"))
}

fn to_cstrings(values: &[String]) -> Result<Vec<CString>, std::ffi::NulError> {
    values
        .iter()
        .map(|value| CString::new(value.as_str()))
        .collect()
}
