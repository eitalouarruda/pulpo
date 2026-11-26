//! Autorização de conexões ao proxy local por árvore de processos (PID).
//! Garante que apenas o próprio Pulpo e seus processos filhos (WebView2/CEF)
//! possam usar o proxy em `127.0.0.1`, sem quebrar o `ProxyResolvingClientSocket`
//! do WebRTC (que não suporta `407 Proxy-Authorization`).

#[cfg(any(windows, target_os = "linux", test))]
use std::collections::HashMap;
#[cfg(windows)]
use std::collections::HashSet;
use std::net::SocketAddr;
#[cfg(windows)]
use std::sync::{Mutex, OnceLock};

#[cfg(any(windows, target_os = "linux", test))]
const MAX_TREE_DEPTH: usize = 8;

#[cfg(windows)]
fn extra_allowed_roots() -> &'static Mutex<HashSet<u32>> {
    static ROOTS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Registra um PID raiz adicional pertencente ao Pulpo (ex.: `BrowserProcessId`
/// de um WebView2) caso o SO tenha criado o processo fora da árvore direta.
#[cfg(windows)]
pub fn register_allowed_root_pid(pid: u32) {
    if pid != 0 {
        extra_allowed_roots().lock().unwrap().insert(pid);
    }
}

#[cfg(windows)]
fn allowed_roots() -> Vec<u32> {
    let mut roots = vec![std::process::id()];
    if let Ok(guard) = extra_allowed_roots().lock() {
        roots.extend(guard.iter().copied());
    }
    roots
}

/// Verifica se `pid` pertence à árvore de qualquer um dos `roots`, subindo pelos
/// pais em `parent_map` com limite de profundidade e proteção contra ciclos.
#[cfg(any(windows, target_os = "linux", test))]
pub(crate) fn is_pid_in_any_tree(pid: u32, roots: &[u32], parent_map: &HashMap<u32, u32>) -> bool {
    if pid == 0 {
        return false;
    }
    let mut current = pid;
    for _ in 0..=MAX_TREE_DEPTH {
        if roots.contains(&current) {
            return true;
        }
        let Some(&parent) = parent_map.get(&current) else {
            return false;
        };
        if parent == 0 || parent == current {
            return false;
        }
        current = parent;
    }
    false
}

/// Confere se a conexão loopback `(peer -> 127.0.0.1:local_port)` pertence ao
/// Pulpo ou a um processo filho autorizado. Falha fechado (`false`) em erro.
pub fn is_authorized_peer(peer: SocketAddr, local_port: u16) -> bool {
    if !peer.ip().is_loopback() {
        return false;
    }
    platform::is_authorized_peer(peer, local_port)
}

#[cfg(windows)]
mod platform {
    use std::collections::HashMap;
    use std::mem::size_of;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use windows::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
    };
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    use super::{allowed_roots, is_pid_in_any_tree};

    const AF_INET: u32 = 2;
    const MAX_TCP_TABLE_BYTES: usize = 16 * 1024 * 1024;

    struct HandleGuard(HANDLE);

    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if !self.0.is_invalid() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: `self.0` é um handle válido obtido de `CreateToolhelp32Snapshot`.
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn lookup_loopback_owner_pid(peer_port: u16, local_port: u16) -> Option<u32> {
        let mut needed: u32 = 0;
        // SAFETY: Consulta apenas o tamanho necessário da tabela TCP (ponteiro nulo).
        let first = unsafe {
            GetExtendedTcpTable(
                None,
                &mut needed,
                false,
                AF_INET,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };
        if first != ERROR_INSUFFICIENT_BUFFER.0 && first != NO_ERROR.0 {
            return None;
        }

        let mut buf_u32: Vec<u32> = Vec::new();
        for _ in 0..3 {
            let size = (needed as usize).max(size_of::<u32>());
            if size > MAX_TCP_TABLE_BYTES {
                return None;
            }
            // Alocação em `Vec<u32>` garante alinhamento de 4 bytes para `MIB_TCPTABLE_OWNER_PID`.
            let words = size.div_ceil(size_of::<u32>());
            buf_u32.resize(words, 0);
            needed = (words * size_of::<u32>()) as u32;

            // SAFETY: `buf_u32` tem capacidade válida de `needed` bytes e alinhamento de 4 bytes.
            let res = unsafe {
                GetExtendedTcpTable(
                    Some(buf_u32.as_mut_ptr().cast()),
                    &mut needed,
                    false,
                    AF_INET,
                    TCP_TABLE_OWNER_PID_ALL,
                    0,
                )
            };
            if res == NO_ERROR.0 {
                let byte_len = buf_u32.len() * size_of::<u32>();
                let num_entries = buf_u32[0] as usize;
                let row_size = size_of::<MIB_TCPROW_OWNER_PID>();
                let required = size_of::<u32>().checked_add(num_entries.checked_mul(row_size)?)?;
                if required > byte_len {
                    return None;
                }
                let loopback_nbo = u32::from_ne_bytes(Ipv4Addr::LOCALHOST.octets());
                let base_ptr = buf_u32.as_ptr() as *const u8;
                for i in 0..num_entries {
                    // SAFETY: `required <= byte_len` garante que o índice `i` está dentro de `buf_u32`.
                    let row = unsafe {
                        let ptr = base_ptr.add(size_of::<u32>() + i * row_size)
                            as *const MIB_TCPROW_OWNER_PID;
                        ptr.read_unaligned()
                    };
                    if row.dwLocalAddr != loopback_nbo || row.dwRemoteAddr != loopback_nbo {
                        continue;
                    }
                    let row_local_port = u16::from_be((row.dwLocalPort & 0xFFFF) as u16);
                    let row_remote_port = u16::from_be((row.dwRemotePort & 0xFFFF) as u16);
                    // Buscamos a linha do cliente: porta local = `peer_port`, remota = `local_port`.
                    if row_local_port == peer_port && row_remote_port == local_port {
                        return Some(row.dwOwningPid);
                    }
                }
                return None;
            }
            if res != ERROR_INSUFFICIENT_BUFFER.0 {
                return None;
            }
        }
        None
    }

    fn snapshot_parent_pids() -> Option<HashMap<u32, u32>> {
        // SAFETY: `CreateToolhelp32Snapshot` retorna um handle válido ou erro.
        let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
        let _guard = HandleGuard(handle);

        let mut map = HashMap::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        // SAFETY: `handle` é um snapshot válido e `entry.dwSize` foi inicializado corretamente.
        let mut more = unsafe { Process32FirstW(handle, &mut entry) }.is_ok();
        while more {
            map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            // SAFETY: `handle` continua válido até o fim de `_guard`.
            more = unsafe { Process32NextW(handle, &mut entry) }.is_ok();
        }
        Some(map)
    }

    pub fn is_authorized_peer(peer: SocketAddr, local_port: u16) -> bool {
        if !matches!(peer.ip(), IpAddr::V4(ip) if ip.is_loopback()) {
            return false;
        }
        let Some(owner_pid) = lookup_loopback_owner_pid(peer.port(), local_port) else {
            return false;
        };
        let roots = allowed_roots();
        if roots.contains(&owner_pid) {
            return true;
        }
        let Some(parents) = snapshot_parent_pids() else {
            return false;
        };
        is_pid_in_any_tree(owner_pid, &roots, &parents)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::collections::HashMap;
    use std::fs;
    use std::net::{IpAddr, SocketAddr};

    use super::is_pid_in_any_tree;

    fn lookup_loopback_inode(peer_port: u16, local_port: u16) -> Option<String> {
        let content = fs::read_to_string("/proc/net/tcp").ok()?;
        let expected_local = format!("0100007F:{peer_port:04X}");
        let expected_remote = format!("0100007F:{local_port:04X}");
        for line in content.lines().skip(1) {
            let mut cols = line.split_whitespace();
            let _slot = cols.next()?;
            let local = cols.next()?;
            let remote = cols.next()?;
            if local.eq_ignore_ascii_case(&expected_local)
                && remote.eq_ignore_ascii_case(&expected_remote)
            {
                // Colunas: 0:sl 1:local 2:rem 3:st 4:tx/rx 5:tr/tm 6:retrnsmt 7:uid 8:timeout 9:inode
                let inode = cols.nth(6)?;
                if inode != "0" {
                    return Some(format!("socket:[{inode}]"));
                }
            }
        }
        None
    }

    fn read_parent_pids() -> HashMap<u32, u32> {
        let mut map = HashMap::new();
        let Ok(entries) = fs::read_dir("/proc") else {
            return map;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
                continue;
            };
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("PPid:") {
                    if let Ok(ppid) = rest.trim().parse::<u32>() {
                        map.insert(pid, ppid);
                    }
                    break;
                }
            }
        }
        map
    }

    fn pid_owns_socket(pid: u32, needle: &str) -> bool {
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            return false;
        };
        for fd in fds.flatten() {
            if let Ok(target) = fs::read_link(fd.path()) {
                if target.to_string_lossy() == needle {
                    return true;
                }
            }
        }
        false
    }

    pub fn is_authorized_peer(peer: SocketAddr, local_port: u16) -> bool {
        if !matches!(peer.ip(), IpAddr::V4(ip) if ip.is_loopback()) {
            return false;
        }
        let Some(socket_link) = lookup_loopback_inode(peer.port(), local_port) else {
            return false;
        };
        let roots = [std::process::id()];
        let parents = read_parent_pids();
        for &pid in parents.keys() {
            if is_pid_in_any_tree(pid, &roots, &parents) && pid_owns_socket(pid, &socket_link) {
                return true;
            }
        }
        false
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use std::net::SocketAddr;

    pub fn is_authorized_peer(peer: SocketAddr, _local_port: u16) -> bool {
        peer.ip().is_loopback()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_tree_accepts_root_and_descendants() {
        let mut parents = HashMap::new();
        parents.insert(100, 1);
        parents.insert(200, 100);
        parents.insert(300, 200);
        parents.insert(999, 1);

        assert!(is_pid_in_any_tree(100, &[100], &parents));
        assert!(is_pid_in_any_tree(200, &[100], &parents));
        assert!(is_pid_in_any_tree(300, &[100], &parents));
        assert!(!is_pid_in_any_tree(999, &[100], &parents));
        assert!(!is_pid_in_any_tree(0, &[100], &parents));
    }

    #[test]
    fn pid_tree_handles_cycles_safely() {
        let mut parents = HashMap::new();
        parents.insert(10, 20);
        parents.insert(20, 10);
        assert!(!is_pid_in_any_tree(10, &[99], &parents));
    }

    #[tokio::test]
    async fn loopback_connection_from_current_process_is_authorized() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_port = listener.local_addr().unwrap().port();
        let client = tokio::net::TcpStream::connect(("127.0.0.1", local_port))
            .await
            .unwrap();
        let (_server, peer) = listener.accept().await.unwrap();
        assert!(is_authorized_peer(peer, local_port));
        drop(client);
    }
}
