//! Pin the current thread to one CPU (Linux only).
//!
//! Without pinning the OS may move a thread between cores at any time. A
//! migration throws away that core's warm caches and shows up as a latency
//! spike, so a clean latency run pins the caller and the engine thread to two
//! different physical cores. macOS offers no equivalent hard pin.

#[cfg(target_os = "linux")]
pub fn pin_current_thread(cpu: usize) -> Result<(), String> {
    // SAFETY: `cpu_set_t` is plain data, so all-zeroes is a valid empty set.
    // `sched_setaffinity(0, ..)` applies to the calling thread only and reads
    // from the set we pass for exactly the size we give it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn pin_current_thread(_cpu: usize) -> Result<(), String> {
    Err("thread pinning is only supported on Linux".into())
}

/// The CPUs that share a physical core with `cpu` (including itself), read from
/// sysfs, e.g. "0,2". `None` where the information is not available.
pub fn thread_siblings(cpu: usize) -> Option<Vec<usize>> {
    let text = std::fs::read_to_string(format!(
        "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
    ))
    .ok()?;
    let mut out = Vec::new();
    for part in text.trim().split(',') {
        match part.split_once('-') {
            Some((a, b)) => out.extend(a.parse::<usize>().ok()?..=b.parse::<usize>().ok()?),
            None => out.push(part.parse().ok()?),
        }
    }
    Some(out)
}
