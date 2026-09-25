//! Ghostty's native scripting API, driven with raw Apple Events so title
//! updates never share the terminal's output stream. Events target the Ghostty
//! process that hosts the session by pid, which never relaunches a Ghostty the
//! user has quit.

use anyhow::{Result, bail, ensure};
use std::ffi::{c_long, c_void};
use std::path::Path;
use std::ptr;

type DescType = u32;
type OSErr = i16;

const WAIT_REPLY_NEVER_INTERACT_DONT_RECONNECT: i32 = 0x03 | 0x10 | 0x80;
const REPLY_TIMEOUT_TICKS: c_long = 60;
const AUTO_GENERATE_RETURN_ID: i16 = -1;
const ANY_TRANSACTION_ID: i32 = 0;

const fn code(text: [u8; 4]) -> u32 {
    u32::from_be_bytes(text)
}

#[repr(C, packed(2))]
struct RawDesc {
    descriptor_type: DescType,
    data_handle: *mut c_void,
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn AECreateDesc(
        type_code: DescType,
        data: *const c_void,
        size: isize,
        result: *mut RawDesc,
    ) -> OSErr;
    fn AECreateList(
        factoring: *const c_void,
        factored_size: isize,
        is_record: u8,
        result: *mut RawDesc,
    ) -> OSErr;
    fn AECoerceDesc(desc: *const RawDesc, to_type: DescType, result: *mut RawDesc) -> OSErr;
    fn AECreateAppleEvent(
        event_class: u32,
        event_id: u32,
        target: *const RawDesc,
        return_id: i16,
        transaction_id: i32,
        result: *mut RawDesc,
    ) -> OSErr;
    fn AEPutParamDesc(event: *mut RawDesc, keyword: u32, desc: *const RawDesc) -> OSErr;
    fn AESendMessage(
        event: *const RawDesc,
        reply: *mut RawDesc,
        send_mode: i32,
        timeout_ticks: c_long,
    ) -> i32;
    fn AEGetParamDesc(
        event: *const RawDesc,
        keyword: u32,
        desired_type: DescType,
        result: *mut RawDesc,
    ) -> OSErr;
    fn AECountItems(list: *const RawDesc, count: *mut c_long) -> OSErr;
    fn AEGetNthDesc(
        list: *const RawDesc,
        index: c_long,
        desired_type: DescType,
        keyword: *mut u32,
        result: *mut RawDesc,
    ) -> OSErr;
    fn AEGetDescDataSize(desc: *const RawDesc) -> isize;
    fn AEGetDescData(desc: *const RawDesc, data: *mut c_void, max_size: isize) -> OSErr;
    fn AEDisposeDesc(desc: *mut RawDesc) -> OSErr;
}

fn check(status: OSErr, action: &str) -> Result<()> {
    ensure!(
        status == 0,
        "{action} failed with Apple Event Manager error {status}"
    );
    Ok(())
}

struct Desc(RawDesc);

impl Desc {
    fn null() -> Self {
        Self(RawDesc {
            descriptor_type: code(*b"null"),
            data_handle: ptr::null_mut(),
        })
    }

    fn from_bytes(type_code: DescType, bytes: &[u8]) -> Result<Self> {
        let mut desc = Self::null();
        let size = isize::try_from(bytes.len())?;
        check(
            unsafe { AECreateDesc(type_code, bytes.as_ptr().cast(), size, &raw mut desc.0) },
            "creating a descriptor",
        )?;
        Ok(desc)
    }

    fn four_char(type_code: DescType, value: [u8; 4]) -> Result<Self> {
        Self::from_bytes(type_code, &code(value).to_ne_bytes())
    }

    fn text(text: &str) -> Result<Self> {
        Self::from_bytes(code(*b"utf8"), text.as_bytes())
    }

    fn process(pid: u32) -> Result<Self> {
        Self::from_bytes(code(*b"kpid"), &i32::try_from(pid)?.to_ne_bytes())
    }

    fn specifier(want: [u8; 4], form: [u8; 4], selection: &Self, container: &Self) -> Result<Self> {
        let mut record = Self::null();
        check(
            unsafe { AECreateList(ptr::null(), 0, 1, &raw mut record.0) },
            "creating an object specifier",
        )?;
        for (keyword, value) in [
            (*b"want", &Self::four_char(code(*b"type"), want)?),
            (*b"form", &Self::four_char(code(*b"enum"), form)?),
            (*b"seld", selection),
            (*b"from", container),
        ] {
            check(
                unsafe { AEPutParamDesc(&raw mut record.0, code(keyword), &raw const value.0) },
                "filling an object specifier",
            )?;
        }
        record.coerce(*b"obj ")
    }

    fn coerce(&self, to_type: [u8; 4]) -> Result<Self> {
        let mut result = Self::null();
        check(
            unsafe { AECoerceDesc(&raw const self.0, code(to_type), &raw mut result.0) },
            "coercing a descriptor",
        )?;
        Ok(result)
    }

    fn data(&self) -> Result<Vec<u8>> {
        let size = unsafe { AEGetDescDataSize(&raw const self.0) };
        let mut data = vec![0; usize::try_from(size)?];
        check(
            unsafe { AEGetDescData(&raw const self.0, data.as_mut_ptr().cast(), size) },
            "reading a descriptor",
        )?;
        Ok(data)
    }

    fn string(&self) -> Result<String> {
        Ok(String::from_utf8(self.coerce(*b"utf8")?.data()?)?)
    }

    fn boolean(&self) -> Result<bool> {
        Ok(self
            .coerce(*b"bool")?
            .data()?
            .first()
            .is_some_and(|value| *value != 0))
    }

    fn items(&self) -> Result<Vec<Self>> {
        let mut count: c_long = 0;
        check(
            unsafe { AECountItems(&raw const self.0, &raw mut count) },
            "counting a list",
        )?;
        (1..=count)
            .map(|index| {
                let mut item = Self::null();
                let mut keyword = 0;
                check(
                    unsafe {
                        AEGetNthDesc(
                            &raw const self.0,
                            index,
                            code(*b"****"),
                            &raw mut keyword,
                            &raw mut item.0,
                        )
                    },
                    "reading a list item",
                )?;
                Ok(item)
            })
            .collect()
    }

    fn parameter(&self, keyword: [u8; 4]) -> Option<Self> {
        let mut result = Self::null();
        let status = unsafe {
            AEGetParamDesc(
                &raw const self.0,
                code(keyword),
                code(*b"****"),
                &raw mut result.0,
            )
        };
        (status == 0).then_some(result)
    }
}

impl Drop for Desc {
    fn drop(&mut self) {
        unsafe { AEDisposeDesc(&raw mut self.0) };
    }
}

fn send(
    pid: u32,
    event_class: [u8; 4],
    event_id: [u8; 4],
    parameters: &[([u8; 4], &Desc)],
) -> Result<Desc> {
    let target = Desc::process(pid)?;
    let mut event = Desc::null();
    check(
        unsafe {
            AECreateAppleEvent(
                code(event_class),
                code(event_id),
                &raw const target.0,
                AUTO_GENERATE_RETURN_ID,
                ANY_TRANSACTION_ID,
                &raw mut event.0,
            )
        },
        "creating an Apple event",
    )?;
    for (keyword, value) in parameters {
        check(
            unsafe { AEPutParamDesc(&raw mut event.0, code(*keyword), &raw const value.0) },
            "adding an Apple event parameter",
        )?;
    }
    let mut reply = Desc::null();
    let status = unsafe {
        AESendMessage(
            &raw const event.0,
            &raw mut reply.0,
            WAIT_REPLY_NEVER_INTERACT_DONT_RECONNECT,
            REPLY_TIMEOUT_TICKS,
        )
    };
    ensure!(
        status == 0,
        "Ghostty did not answer the Apple event (error {status})"
    );
    if let Some(failure) = reply.parameter(*b"errn") {
        let failure = i32::from_ne_bytes(
            failure
                .coerce(*b"long")?
                .data()?
                .try_into()
                .unwrap_or_default(),
        );
        ensure!(failure == 0, "Ghostty Apple event error {failure}");
    }
    reply
        .parameter(*b"----")
        .ok_or_else(|| anyhow::anyhow!("Ghostty's reply had no result"))
}

fn surfaces(pid: u32) -> Result<Vec<Desc>> {
    let every = Desc::four_char(code(*b"abso"), *b"all ")?;
    let terminals = Desc::specifier(*b"Gtrm", *b"indx", &every, &Desc::null())?;
    send(pid, *b"core", *b"getd", &[(*b"----", &terminals)])?.items()
}

fn surface_tty(pid: u32, surface: &Desc) -> Result<String> {
    let property = Desc::four_char(code(*b"type"), *b"Gtty")?;
    let tty = Desc::specifier(*b"prop", *b"prop", &property, surface)?;
    send(pid, *b"core", *b"getd", &[(*b"----", &tty)])?.string()
}

pub(crate) struct Connection {
    pid: u32,
    surface: Desc,
}

impl Connection {
    pub(crate) fn open(tty: &Path, pid: u32) -> Result<Self> {
        for surface in surfaces(pid)? {
            if surface_tty(pid, &surface)? == tty.to_string_lossy() {
                return Ok(Self { pid, surface });
            }
        }
        bail!("Ghostty terminal not found for {}", tty.display())
    }

    pub(crate) fn write(&mut self, title: &str) -> Result<()> {
        let action = Desc::text(&format!("set_surface_title:{title}"))?;
        let accepted = send(
            self.pid,
            *b"Ghst",
            *b"PfAc",
            &[(*b"----", &action), (*b"GonT", &self.surface)],
        )?
        .boolean()?;
        ensure!(accepted, "Ghostty rejected the title update");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn specifiers_carry_their_class_form_and_selection() {
        let property = Desc::four_char(code(*b"type"), *b"Gtty").unwrap();
        let specifier = Desc::specifier(*b"prop", *b"prop", &property, &Desc::null()).unwrap();
        let descriptor_type = specifier.0.descriptor_type;
        assert_eq!(descriptor_type, code(*b"obj "));
        let mut want = Desc::null();
        assert_eq!(
            unsafe {
                AEGetParamDesc(
                    &raw const specifier.0,
                    code(*b"want"),
                    code(*b"type"),
                    &raw mut want.0,
                )
            },
            0
        );
        assert_eq!(want.data().unwrap(), code(*b"prop").to_ne_bytes());
    }

    #[test]
    fn titles_are_sent_as_unicode_data() {
        for title in ["⠋ Working | 漢字", "✳ Ready | \"quoted\" \\ path", ""] {
            assert_eq!(Desc::text(title).unwrap().string().unwrap(), title);
        }
    }

    #[test]
    fn an_exited_process_is_not_relaunched_or_awaited() {
        let mut exited = Command::new("/usr/bin/true").spawn().unwrap();
        exited.wait().unwrap();
        let started = Instant::now();
        assert!(surfaces(exited.id()).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    #[ignore = "requires a local Ghostty installation"]
    fn a_quit_ghostty_is_not_relaunched_for_a_title_update() {
        struct GhosttyProcess(std::process::Child);
        impl Drop for GhosttyProcess {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut ghostty = GhosttyProcess(
            Command::new("/Applications/Ghostty.app/Contents/MacOS/ghostty")
                .args([
                    "--config-default-files=false",
                    "--initial-window=false",
                    "--window-save-state=never",
                    "--auto-update=off",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let pid = ghostty.0.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Err(error) = surfaces(pid) {
            assert!(
                Instant::now() < deadline,
                "native connection failed: {error}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        ghostty.0.kill().unwrap();
        ghostty.0.wait().unwrap();
        let started = Instant::now();
        assert!(surfaces(pid).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
