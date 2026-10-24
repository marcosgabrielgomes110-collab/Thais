#![forbid(unsafe_code)]

//! Landlock sandbox helper used by the Bash tool on Linux.

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::main()
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("thais-sandbox is only available on Linux")
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Command;

    use anyhow::{Context, bail};
    use landlock::{
        ABI, Access, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        path_beneath_rules,
    };

    pub fn main() -> anyhow::Result<()> {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        let mut workspace: Option<PathBuf> = None;
        let mut cwd: Option<PathBuf> = None;
        let mut command: Option<String> = None;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--workspace" => {
                    index += 1;
                    workspace = args.get(index).map(PathBuf::from);
                }
                "--cwd" => {
                    index += 1;
                    cwd = args.get(index).map(PathBuf::from);
                }
                "--" => {
                    command = args.get(index + 1).cloned();
                    break;
                }
                other => bail!("unknown argument: {other}"),
            }
            index += 1;
        }
        let workspace = workspace.context("missing --workspace")?;
        let cwd = cwd.unwrap_or_else(|| workspace.clone());
        let command = command.context("missing shell command")?;

        let abi = ABI::V4;
        let status = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))?
            .handle_access(AccessNet::ConnectTcp)?
            .handle_access(AccessNet::BindTcp)?
            .create()?
            .add_rules(path_beneath_rules(["/"], AccessFs::from_read(abi)))?
            .add_rules(path_beneath_rules(
                [workspace.as_path()],
                AccessFs::from_all(abi),
            ))?
            .restrict_self()?;
        match status.ruleset {
            RulesetStatus::FullyEnforced | RulesetStatus::PartiallyEnforced => {}
            RulesetStatus::NotEnforced => bail!("Landlock is not enforced by this kernel"),
        }
        apply_seccomp()?;

        let error = Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .exec();
        Err(anyhow::anyhow!("could not exec shell: {error}"))
    }

    fn apply_seccomp() -> anyhow::Result<()> {
        use seccompiler::{SeccompAction, SeccompFilter};
        use std::convert::TryInto;
        let denied = [
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_ptrace,
            libc::SYS_bpf,
            libc::SYS_keyctl,
            libc::SYS_reboot,
            libc::SYS_kexec_load,
            libc::SYS_init_module,
            libc::SYS_finit_module,
            libc::SYS_delete_module,
            libc::SYS_syslog,
            libc::SYS_acct,
            libc::SYS_swapon,
            libc::SYS_swapoff,
        ];
        let rules = denied
            .into_iter()
            .map(|syscall| (syscall, vec![]))
            .collect();
        let filter: seccompiler::BpfProgram = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPERM as u32),
            std::env::consts::ARCH.try_into()?,
        )?
        .try_into()?;
        seccompiler::apply_filter(&filter)?;
        Ok(())
    }
}
