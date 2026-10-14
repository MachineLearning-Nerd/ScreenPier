use super::{Cancellation, DesktopError, PointerPreparationFault, Result};
use rustix::net::{self, AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use std::{
    os::fd::OwnedFd,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const READY: &[u8] = b"twa_pointer_ready_v1";
const UNSUPPORTED: &[u8] = b"unsupported";
const CAPABLE: &[u8] = b"twa_pointer_capable_v1";
const REFRESH: &[u8] = b"repl local function refresh() local p=hl.get_cursor_pos(); local r=hl.dispatch(hl.dsp.cursor.move({x=p.x,y=p.y})); if type(r)~=\"table\" or r.ok~=true then error(\"refresh failed\") end; return \"twa_pointer_ready_v1\" end return refresh()";
const CAPABILITY: &[u8] = b"repl if type(hl)==\"table\" and type(hl.get_cursor_pos)==\"function\" and type(hl.dispatch)==\"function\" and type(hl.dsp)==\"table\" and type(hl.dsp.cursor)==\"table\" and type(hl.dsp.cursor.move)==\"function\" then return \"twa_pointer_capable_v1\" end return \"unsupported\"";

pub(super) enum PointerAdapter {
    Generic,
    Hyprland(PathBuf),
    Unsupported,
    UnobservedHyprland(Option<PathBuf>),
}
impl PointerAdapter {
    pub(super) fn detect(cancellation: &Cancellation) -> Result<Self> {
        cancellation.check()?;
        let Some(signature) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") else {
            return Ok(Self::Generic);
        };
        let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
            return Ok(Self::UnobservedHyprland(None));
        };
        let Ok(endpoint) = endpoint(Path::new(&runtime), Path::new(&signature)) else {
            return Ok(Self::UnobservedHyprland(None));
        };
        Self::detect_endpoint(
            endpoint,
            cancellation,
            Instant::now() + Duration::from_secs(2),
        )
    }
    pub(super) fn detect_endpoint(
        endpoint: PathBuf,
        cancellation: &Cancellation,
        deadline: Instant,
    ) -> Result<Self> {
        cancellation.check()?;
        let probe = (|| {
            let mut reply = Reply::prepare(&endpoint)?;
            reply.start(CAPABILITY)?;
            loop {
                cancellation.check()?;
                if Instant::now() >= deadline {
                    return Err(DesktopError::Timeout);
                }
                if let Some(index) = reply.poll(&[CAPABLE, UNSUPPORTED])? {
                    return Ok(index == 0);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        })();
        match probe {
            Ok(true) => Ok(Self::Hyprland(endpoint)),
            Ok(false) => Ok(Self::Unsupported),
            Err(DesktopError::Cancelled) => Err(DesktopError::Cancelled),
            Err(_) => {
                cancellation.check()?;
                Ok(Self::UnobservedHyprland(Some(endpoint)))
            }
        }
    }
    pub(super) fn prepare(&self) -> Result<Option<Reply>> {
        match self {
            Self::Generic => Ok(None),
            Self::Hyprland(path) | Self::UnobservedHyprland(Some(path)) => {
                Reply::prepare(path).map(Some)
            }
            Self::UnobservedHyprland(None) => Err(fault(PointerPreparationFault::Unavailable)),
            Self::Unsupported => Err(fault(PointerPreparationFault::Unsupported)),
        }
    }
}
fn endpoint(runtime: &Path, signature: &Path) -> Result<PathBuf> {
    let metadata = std::fs::metadata(runtime)?;
    if !runtime.is_absolute()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::getuid().as_raw()
    {
        return Err(fault(PointerPreparationFault::Peer));
    }
    let mut components = signature.components();
    if signature.file_name() != Some(signature.as_os_str())
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(fault(PointerPreparationFault::Unsupported));
    }
    Ok(runtime.join("hypr").join(signature).join(".socket.sock"))
}
fn fault(kind: PointerPreparationFault) -> DesktopError {
    DesktopError::PointerPreparation(kind)
}
pub(super) struct Reply {
    socket: OwnedFd,
    address: SocketAddrUnix,
    bytes: Vec<u8>,
    #[cfg(test)]
    pub(super) write_limit: Option<usize>,
}
impl Reply {
    pub(super) fn prepare(endpoint: &Path) -> Result<Self> {
        let address = SocketAddrUnix::new(endpoint).map_err(std::io::Error::from)?;
        let socket = net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )
        .map_err(std::io::Error::from)?;
        Ok(Self {
            socket,
            address,
            bytes: Vec::with_capacity(32),
            #[cfg(test)]
            write_limit: None,
        })
    }
    fn start(&mut self, request: &[u8]) -> Result<()> {
        net::connect(&self.socket, &self.address)
            .map_err(|_| fault(PointerPreparationFault::Unavailable))?;
        if net::sockopt::socket_peercred(&self.socket)
            .map_err(std::io::Error::from)?
            .uid
            != rustix::process::getuid()
        {
            return Err(fault(PointerPreparationFault::Peer));
        }
        #[cfg(test)]
        let wire = match self.write_limit {
            Some(usize::MAX) => return Err(fault(PointerPreparationFault::Unavailable)),
            Some(limit) => &request[..limit],
            None => request,
        };
        #[cfg(not(test))]
        let wire = request;
        let count = net::send(&self.socket, wire, net::SendFlags::NOSIGNAL)
            .map_err(|_| fault(PointerPreparationFault::Unavailable))?;
        if count != request.len() {
            return Err(fault(PointerPreparationFault::ShortWrite));
        }
        Ok(())
    }
    pub(super) fn refresh(&mut self) -> Result<()> {
        self.start(REFRESH)
    }
    pub(super) fn ready(&mut self) -> Result<bool> {
        self.poll(&[READY]).map(|reply| reply.is_some())
    }
    fn poll(&mut self, expected: &[&[u8]]) -> Result<Option<usize>> {
        let mut bytes = [0u8; 32];
        match net::recv(&self.socket, &mut bytes[..], net::RecvFlags::DONTWAIT) {
            Ok((0, _)) => expected
                .iter()
                .position(|reply| self.bytes == *reply)
                .map(Some)
                .ok_or_else(|| fault(PointerPreparationFault::InvalidReply)),
            Ok((count, _)) => {
                if self.bytes.len() + count
                    > expected.iter().map(|reply| reply.len()).max().unwrap()
                {
                    return Err(fault(PointerPreparationFault::InvalidReply));
                }
                self.bytes.extend_from_slice(&bytes[..count]);
                if !expected.iter().any(|reply| reply.starts_with(&self.bytes)) {
                    return Err(fault(PointerPreparationFault::InvalidReply));
                }
                Ok(None)
            }
            Err(rustix::io::Errno::AGAIN) => Ok(None),
            Err(_) => Err(fault(PointerPreparationFault::InvalidReply)),
        }
    }
}
