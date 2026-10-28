//! "Open at Login": entrada em `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//!
//! Não requer privilégios de administrador (é a chave do utilizador atual) e é
//! o mecanismo recomendado para arranque automático de aplicações de bandeja.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RRF_RT_REG_SZ,
};

use crate::winutil::{wide, RUN_SUBKEY, RUN_VALUE};

pub fn is_enabled() -> bool {
    let sub = wide(RUN_SUBKEY);
    let name = wide(RUN_VALUE);
    let mut buf = [0u8; 2048];
    let mut size = buf.len() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let sub = wide(RUN_SUBKEY);
    let name = wide(RUN_VALUE);

    let mut hkey = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("RegCreateKeyEx falhou ({status:?})"));
    }

    let result = if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        // Aspas obrigatórias: o caminho pode conter espaços.
        let command = format!("\"{}\"", exe.display());
        let mut data: Vec<u16> = command.encode_utf16().collect();
        data.push(0);
        let bytes =
            unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };
        let status =
            unsafe { RegSetValueExW(hkey, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("RegSetValueEx falhou ({status:?})"))
        }
    } else {
        let status = unsafe { RegDeleteValueW(hkey, PCWSTR(name.as_ptr())) };
        if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("RegDeleteValue falhou ({status:?})"))
        }
    };

    unsafe {
        let _ = RegCloseKey(hkey);
    }
    result
}
