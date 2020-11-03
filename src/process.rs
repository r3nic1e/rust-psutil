//! Read process-specific information from `/proc`
//!
//! More information about specific fields can be found in
//! [proc(5)](http://man7.org/linux/man-pages/man5/proc.5.html).
//!
//! ### Field sizes
//!
//! The manual pages for `proc` define integer sizes using `scanf(3)` format
//! specifiers, which parse to implementation specific sizes. This is obviously
//! a terrible idea, and so this code makes some assumptions about the sizes of
//! those specifiers.
//!
//! These assumptions are backed up by `libc::types::os::arch::posix88::pid_t`,
//! which declares PIDs to be signed 32 bit integers. `proc(5)` declares that
//! PIDs use the `%d` format specifier.
//!
//! - `%d` / `%u` - 32 bit signed and unsigned integers
//! - `%ld` / `%lu` - 64 bit signed and unsigned integers
//!
//! **WARNING**: Rust currently has no support for 128 bit integers^[rfc521]
//! so `%llu` (used by the `starttime` and `delayacct_blkio_ticks` fields) is is
//! instead represented by a 64 bit integer, with the hope that doesn't break.
//!
//! ### CPU time fields and clock ticks
//!

//! The CPU time fields are very strange. Inside the Linux kernel they each use
//! the same type^[array.c:361] but when printed use different
//! types^[array.c:456] - the fields `utime`, `stime` and `gtime` are
//! unsigned integers, whereas `cutime`, `cstime` and `cgtime` are signed
//! integers.

//!
//! These values are all returned as a number of clock ticks, which can be
//! divided by `sysconf(_SC_CLK_TCK)` to get a value in seconds. The `Process`
//! struct does this conversion automatically, and all CPU time fields use the
//! `f64` type.
//!
//! [rfc521]: https://github.com/rust-lang/rfcs/issues/521
//! [array.c:361]: https://github.com/torvalds/linux/blob/4f671fe2f9523a1ea206f63fe60a7c7b3a56d5c7/fs/proc/array.c#L361
//! [array.c:456]: https://github.com/torvalds/linux/blob/4f671fe2f9523a1ea206f63fe60a7c7b3a56d5c7/fs/proc/array.c#L456
//!

use std::fs::{self,read_dir,read_link};
use std::os::unix::fs::MetadataExt;
use std::io::{Error,ErrorKind,Result};
use std::path::{Path,PathBuf};
use std::str::FromStr;
use std::string::ToString;
use std::vec::Vec;

use libc::{c_long};
use libc::consts::os::sysconf::{_SC_CLK_TCK,_SC_PAGESIZE};
use libc::funcs::posix88::unistd::sysconf;

use ::{PID,UID,GID};
use ::pidfile::read_pidfile;
use ::utils::read_file;

lazy_static! {
    static ref TICKS_PER_SECOND: c_long = {
        unsafe { sysconf(_SC_CLK_TCK) }
    };
    static ref PAGE_SIZE: c_long = {
        unsafe { sysconf(_SC_PAGESIZE) }
    };
}

fn procfs_path(pid: super::PID, name: &str) -> PathBuf {
    let mut path = PathBuf::new();
    path.push("/proc");
    path.push(&pid.to_string());
    path.push(&name);
    return path;
}

/// Read a process' file from procfs - `/proc/[pid]/[name]`
fn procfs(pid: super::PID, name: &str) -> Result<String> {
    return read_file(&procfs_path(pid, name));
}

/// Possible statuses for a process
#[derive(Clone,Copy,Debug)]
pub enum State {
    Running,
    Sleeping,
    Waiting,
    Stopped,
    Traced,
    Paging,
    Dead,
    Zombie,
    Idle,
}

impl State {
    /// Returns a State based on a status character from `/proc/[pid]/stat`
    ///
    /// See http://lxr.free-electrons.com/source/fs/proc/array.c#L115
    fn from_char(state: char) -> Result<Self> {
        match state {
            'R' => Ok(State::Running),
            'S' => Ok(State::Sleeping),
            'D' => Ok(State::Waiting),
            'T' => Ok(State::Stopped),
            't' => Ok(State::Traced),
            'W' => Ok(State::Paging),
            'Z' => Ok(State::Zombie),
            'X' => Ok(State::Dead),
            'I' => Ok(State::Idle),
             _  => Err(Error::new(ErrorKind::Other, format!("Invalid state character: {}", state)))
        }
    }
}

impl FromStr for State {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if !s.len() == 1 {
            Err(Error::new(ErrorKind::Other, "State is not a single character"))
        } else {
            State::from_char(s.chars().nth(0).unwrap())
        }
    }
}

impl ToString for State {
    fn to_string(&self) -> String {
        match self {
            &State::Running  => "R".to_string(),
            &State::Sleeping => "S".to_string(),
            &State::Waiting  => "D".to_string(),
            &State::Stopped  => "T".to_string(),
            &State::Traced   => "t".to_string(),
            &State::Paging   => "W".to_string(),
            &State::Zombie   => "Z".to_string(),
            &State::Dead     => "X".to_string(),
            &State::Idle     => "I".to_string(),
        }
    }
}

/// Memory usage of a process
///
/// Read from `/proc/[pid]/statm`
#[derive(Clone,Copy,Debug)]
pub struct Memory {
    /// Total program size (bytes)
    pub size: u64,

    /// Resident Set Size (bytes)
    pub resident: u64,

    /// Shared pages (bytes)
    pub share: u64,

    /// Text
    pub text: u64,

    // /// Library (unused)
    // pub lib: u64,

    /// Data + stack
    pub data: u64,

    // /// Dirty pages (unused)
    // pub dt: u65
}

impl Memory {
    fn new(pid: PID) -> Result<Memory> {
        let statm = try!(procfs(pid, "statm"));
        let bytes: Vec<u64> = statm
            .trim_right()
            .split(" ")
            .map(|n| n.parse().unwrap())
            .collect();

        return Ok(Memory {
            size:       bytes[0] * *PAGE_SIZE as u64,
            resident:   bytes[1] * *PAGE_SIZE as u64,
            share:      bytes[2] * *PAGE_SIZE as u64,
            text:       bytes[3] * *PAGE_SIZE as u64,
            // lib:     bytes[4] * *PAGE_SIZE as u64,
            // workaround kernel overflow bug
            data:       bytes[5].wrapping_mul(*PAGE_SIZE as u64),
            // dt:      bytes[6] * *PAGE_SIZE as u64
        });
    }
}

/// Information about a process gathered from `/proc/[pid]/stat`.
///
/// **IMPORTANT**: See the module level notes for information on the types used
/// by this struct, as some do not match those used by `/proc/[pid]/stat`.
#[derive(Clone,Debug)]
pub struct Process {
    /// PID of the process
    pub pid: PID,

    /// UID of the process
    pub uid: UID,

    /// UID of the process
    pub gid: GID,

    /// Filename of the executable
    pub comm: String,

    /// State of the process as an enum
    pub state: State,

    /// PID of the parent process
    pub ppid: PID,

    /// Process group ID
    pub pgrp: i32,

    /// Session ID
    pub session: i32,

    /// Controlling terminal of the process [TODO: Actually two numbers]
    pub tty_nr: i32,

    /// ID of the foreground group of the controlling terminal
    pub tpgid: i32,

    /// Kernel flags for the process
    pub flags: u32,

    /// Minor faults
    pub minflt: u64,

    /// Minor faults by child processes
    pub cminflt: u64,

    /// Major faults
    pub majflt: u64,

    /// Major faults by child processes
    pub cmajflt: u64,

    /// Time scheduled in user mode (seconds)
    pub utime: f64,

    /// Time scheduled in kernel mode (seconds)
    pub stime: f64,

    /// Time waited-for child processes were scheduled in user mode (seconds)
    pub cutime: f64,

    /// Time waited-for child processes were scheduled in kernel mode (seconds)
    pub cstime: f64,

    /// Priority value (-100..-2 | 0..39)
    pub priority: i64,

    /// Nice value (-20..19)
    pub nice: i64,

    /// Number of threads in the process
    pub num_threads: i64,

    // /// Unmaintained field since linux 2.6.17, always 0
    // itrealvalue: i64,

    /// Time the process was started after system boot (clock ticks)
    pub starttime: u64,

    /// Virtual memory size in bytes
    pub vsize: u64,

    /// Resident Set Size (bytes)
    pub rss: i64,

    /// Current soft limit on process RSS (bytes)
    pub rsslim: u64,

    // These values are memory addresses
    startcode: u64,
    endcode: u64,
    startstack: u64,
    kstkesp: u64,
    kstkeip: u64,

    // /// Signal bitmaps
    // /// These are obselete, use `/proc/[pid]/status` instead
    // signal: u64,
    // blocked: u64,
    // sigignore: u64,
    // sigcatch: u64,

    /// Channel the process is waiting on (address of a system call)
    pub wchan: u64,

    // /// Number of pages swapped (not maintained)
    // pub nswap: u64,

    // /// Number of pages swapped for child processes (not maintained)
    // pub cnswap: u64,

    /// Signal sent to parent when process dies
    pub exit_signal: i32,

    /// Number of the CPU the process was last executed on
    pub processor: i32,

    /// Real-time scheduling priority (0 | 1..99)
    pub rt_priority: u32,

    /// Scheduling policy
    pub policy: u32,

    /// Aggregated block I/O delays (clock ticks)
    pub delayacct_blkio_ticks: u64,

    /// Guest time of the process (seconds)
    pub guest_time: f64,

    /// Guest time of the process's children (seconds)
    pub cguest_time: f64,

    // More memory addresses
    start_data: u64,
    end_data: u64,
    start_brk: u64,
    arg_start: u64,
    arg_end: u64,
    env_start: u64,
    env_end: u64,

    /// The thread's exit status
    pub exit_code: i32
}

/// TODO: This should use `try!` instead of `unwrap()`
macro_rules! from_str { ($field:expr) => (FromStr::from_str($field).unwrap()) }

impl Process {
    /// Parses a process name
    ///
    /// Process names are surrounded by `()` characters, which are removed.
    fn parse_comm(s: &str) -> String {
        s[1..s.len()].to_string()
    }

    /// Attempts to read process information from `/proc/[pid]/stat`.
    ///
    /// `/stat` is seperated by spaces and contains a trailing newline.
    ///
    /// This should return a psutil/process specific error type, so that  errors
    /// can be raised by `FromStr` too
    pub fn new(pid: PID) -> Result<Process> {
        let path = procfs_path(pid, "");
        let meta = try!(fs::metadata(path));
        let stat = try!(procfs(pid, "stat"));

        // read pid
        let mut iter = stat.splitn(2, ' ');
        let pid = iter.next().map(str::parse::<PID>).unwrap().unwrap();

        // read command
        let rest = iter.next().unwrap();
        let start_of_cmd = rest.find('(').unwrap();
        let end_of_cmd = rest.rfind(')').unwrap();
        let cmd = Process::parse_comm(&rest[start_of_cmd..end_of_cmd]);

        let stat: Vec<&str> = rest[end_of_cmd+2..].trim_right().split(' ').collect();

        if stat.len() != 50 {
            return Err(Error::new(ErrorKind::Other,
                "Unexpected number of fields from /proc/[pid]/stat"));
        }

        // Read each field into an attribute for a new Process instance
        return Ok(Process {
            pid:                    pid,
            uid:                    meta.uid(),
            gid:                    meta.gid(),
            comm:                   cmd,
            state:                  from_str!(stat[00]),
            ppid:                   from_str!(stat[01]),
            pgrp:                   from_str!(stat[02]),
            session:                from_str!(stat[03]),
            tty_nr:                 from_str!(stat[04]),
            tpgid:                  from_str!(stat[05]),
            flags:                  from_str!(stat[06]),
            minflt:                 from_str!(stat[07]),
            cminflt:                from_str!(stat[8]),
            majflt:                 from_str!(stat[9]),
            cmajflt:                from_str!(stat[10]),
            utime:                  u64::from_str(stat[11]).unwrap() as f64 / *TICKS_PER_SECOND as f64,
            stime:                  u64::from_str(stat[12]).unwrap() as f64 / *TICKS_PER_SECOND as f64,
            cutime:                 i64::from_str(stat[13]).unwrap() as f64 / *TICKS_PER_SECOND as f64,
            cstime:                 i64::from_str(stat[14]).unwrap() as f64 / *TICKS_PER_SECOND as f64,
            priority:               from_str!(stat[15]),
            nice:                   from_str!(stat[16]),
            num_threads:            from_str!(stat[17]),
            // itrealvalue:         from_str!(stat[18]),
            starttime:              from_str!(stat[19]),
            vsize:                  from_str!(stat[20]),
            rss:                    i64::from_str(stat[21]).unwrap() * *PAGE_SIZE as i64,
            rsslim:                 from_str!(stat[22]),
            startcode:              from_str!(stat[23]),
            endcode:                from_str!(stat[24]),
            startstack:             from_str!(stat[25]),
            kstkesp:                from_str!(stat[26]),
            kstkeip:                from_str!(stat[27]),
            // signal:              from_str!(stat[28]),
            // blocked:             from_str!(stat[29]),
            // sigignore:           from_str!(stat[30]),
            // sigcatch:            from_str!(stat[31]),
            wchan:                  from_str!(stat[32]),
            // nswap:               from_str!(stat[33]),
            // cnswap:              from_str!(stat[34]),
            exit_signal:            from_str!(stat[35]),
            processor:              from_str!(stat[36]),
            rt_priority:            from_str!(stat[37]),
            policy:                 from_str!(stat[38]),
            delayacct_blkio_ticks:  from_str!(stat[39]),
            guest_time:             u64::from_str(stat[40]).unwrap() as f64 / *TICKS_PER_SECOND as f64,
            cguest_time:            i64::from_str(stat[41]).unwrap() as f64 / *TICKS_PER_SECOND as f64,
            start_data:             from_str!(stat[42]),
            end_data:               from_str!(stat[43]),
            start_brk:              from_str!(stat[44]),
            arg_start:              from_str!(stat[45]),
            arg_end:                from_str!(stat[46]),
            env_start:              from_str!(stat[47]),
            env_end:                from_str!(stat[48]),
            exit_code:              from_str!(stat[49])
        });
    }

    /// Create a Process by reading it's PID from a pidfile.
    pub fn from_pidfile(path: &Path) -> Result<Process> {
        Process::new(try!(read_pidfile(&path)))
    }

    /// Return `true` if the process was alive at the time it was read.
    pub fn is_alive(&self) -> bool {
        match self.state {
            State::Zombie => false,
            _ => true
        }
    }

    /// Read `/proc/[pid]/cmdline` as a vector.
    ///
    /// Returns `Err` if `/proc/[pid]/cmdline` is empty.
    pub fn cmdline_vec(&self) -> Result<Option<Vec<String>>> {
        let cmdline = try!(procfs(self.pid, "cmdline"));

        if cmdline == "" {
            return Ok(None);
        } else {
            // Split terminator skips empty trailing substrings
            let split = cmdline.split_terminator(
                |c: char| c == '\0' || c == ' ');

            // `split` returns a vector of slices viewing `cmdline`, so they
            // get mapped to actuall strings before being returned as a vector.
            return Ok(Some(split.map(|x| x.to_string()).collect()));
        }
    }

    /// Return the result of `cmdline_vec` as a String.
    pub fn cmdline(&self) -> Result<Option<String>> {
        Ok(try!(self.cmdline_vec()).and_then(|c| Some(c.join(" "))))
    }

    /// Reads `/proc/[pid]/statm` into a struct.
    pub fn memory(&self) -> Result<Memory> {
        Memory::new(self.pid)
    }

    /// Send SIGKILL to the process.
    pub fn kill(&self) -> Result<()> {
        use libc::funcs::posix88::signal::kill;
        use libc::consts::os::posix88::SIGKILL;

        return match unsafe { kill(self.pid, SIGKILL) } {
            0  => Ok(()),
            -1 => Err(Error::last_os_error()),
            _  => unreachable!()
        };
    }

    pub fn cwd(&self) -> Result<PathBuf> {
        read_link(procfs_path(self.pid, "cwd"))
    }

    pub fn exe(&self) -> Result<PathBuf> {
        read_link(procfs_path(self.pid, "exe"))
    }
}

impl PartialEq for Process {
    // Compares processes using their PID and starttime as an indentity
    fn eq(&self, other: &Process) -> bool {
        (self.pid == other.pid) && (self.starttime == other.starttime)
    }
}

/// Return a vector of all processes in /proc
pub fn all() -> Vec<Process> {
    let mut processes = Vec::new();

    for entry in read_dir(&Path::new("/proc")).unwrap() {
        let path = entry.unwrap().path();
        let file_name = path.file_name().unwrap();
        match FromStr::from_str(&file_name.to_string_lossy()) {
            Ok(pid) => { processes.push(Process::new(pid).unwrap()) },
            Err(_)  => ()
        }
    }

    return processes;
}
