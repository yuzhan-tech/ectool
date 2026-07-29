//! Binary printf-style formatter for EigenComm UniLog records.
//! protocols. Walks a format string and substitutes arguments from a
//! 4-byte-aligned little-endian binary stream.

/// Read a little-endian u32 at `offset`, returning `None` if out of bounds.
fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    data.get(offset..offset + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
}

/// Read a 4-byte-aligned length-prefixed string from `data`.
/// Updates `offset` past the string and aligns it to the next 4-byte boundary.
///
/// Exported so callers can short-circuit the common `%.*s`-style payload
/// shape without invoking the full format-string walker.
pub fn read_len_prefixed_string(data: &[u8], offset: &mut usize) -> Option<String> {
    *offset = (*offset + 3) & !3;
    let len = read_u32(data, *offset)? as usize;
    *offset += 4;
    let bytes = data.get(*offset..*offset + len)?;
    let s = String::from_utf8_lossy(bytes).to_string();
    *offset += len;
    *offset = (*offset + 3) & !3;
    Some(s)
}

/// Walk a printf-style format string and substitute arguments from a binary
/// stream (4-byte aligned LE). Supports %d/%i/%u/%x/%X/%s/%.*s/%p/%c plus %%.
///
/// Length modifier `ll` reads 8 bytes for `%lld`/`%llu`/`%llx`/`%llX`; `l`
/// alone reads 4 bytes (matching the trace and UniLog protocols, where
/// `long` is 32-bit). `h`/`hh`/`z` are recognized and skipped.
///
/// For `%s`: tries a 4-byte length-prefixed string first; if that length
/// would overrun the payload, falls back to reading a NUL-terminated string.
pub fn fmt_printf(fmt: &str, data: &[u8]) -> String {
    let mut out = String::new();
    let mut offset = 0;
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        i += 1;
        if i >= chars.len() {
            break;
        }

        if chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }

        while i < chars.len() && "-+ 0#".contains(chars[i]) {
            i += 1;
        }
        if i < chars.len() && chars[i] == '*' {
            offset = (offset + 3) & !3;
            if offset + 4 <= data.len() {
                offset += 4;
            }
            i += 1;
        } else {
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
        }
        if i < chars.len() && chars[i] == '.' {
            i += 1;
            if i < chars.len() && chars[i] == '*' {
                offset = (offset + 3) & !3;
                let prec = if offset + 4 <= data.len() {
                    let v =
                        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0; 4]));
                    offset += 4;
                    v as usize
                } else {
                    0
                };
                i += 1;
                if i < chars.len() && chars[i] == 's' {
                    if offset + prec <= data.len() {
                        let s = String::from_utf8_lossy(&data[offset..offset + prec]);
                        out.push_str(&s);
                        offset += prec;
                    }
                    i += 1;
                    continue;
                }
            } else {
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
        }
        // Declared here (not before the loop) so it auto-resets to false per directive.
        let mut is_long_long = false;
        if i < chars.len() && chars[i] == 'l' {
            i += 1;
            if i < chars.len() && chars[i] == 'l' {
                is_long_long = true;
                i += 1;
            }
        } else if i < chars.len() && (chars[i] == 'h' || chars[i] == 'z') {
            i += 1;
            if i < chars.len() && chars[i] == 'h' {
                i += 1;
            }
        }

        if i >= chars.len() {
            break;
        }

        let spec = chars[i];
        i += 1;

        match spec {
            'd' | 'i' => {
                offset = (offset + 3) & !3;
                if is_long_long {
                    if offset + 8 <= data.len() {
                        let v = i64::from_le_bytes(
                            data[offset..offset + 8].try_into().unwrap_or([0; 8]),
                        );
                        out.push_str(&v.to_string());
                        offset += 8;
                    }
                } else if offset + 4 <= data.len() {
                    let v =
                        i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0; 4]));
                    out.push_str(&v.to_string());
                    offset += 4;
                }
            }
            'u' => {
                offset = (offset + 3) & !3;
                if is_long_long {
                    if offset + 8 <= data.len() {
                        let v = u64::from_le_bytes(
                            data[offset..offset + 8].try_into().unwrap_or([0; 8]),
                        );
                        out.push_str(&v.to_string());
                        offset += 8;
                    }
                } else if offset + 4 <= data.len() {
                    let v =
                        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0; 4]));
                    out.push_str(&v.to_string());
                    offset += 4;
                }
            }
            'x' => {
                offset = (offset + 3) & !3;
                if is_long_long {
                    if offset + 8 <= data.len() {
                        let v = u64::from_le_bytes(
                            data[offset..offset + 8].try_into().unwrap_or([0; 8]),
                        );
                        out.push_str(&format!("{:x}", v));
                        offset += 8;
                    }
                } else if offset + 4 <= data.len() {
                    let v =
                        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0; 4]));
                    out.push_str(&format!("{:x}", v));
                    offset += 4;
                }
            }
            'X' => {
                offset = (offset + 3) & !3;
                if is_long_long {
                    if offset + 8 <= data.len() {
                        let v = u64::from_le_bytes(
                            data[offset..offset + 8].try_into().unwrap_or([0; 8]),
                        );
                        out.push_str(&format!("{:X}", v));
                        offset += 8;
                    }
                } else if offset + 4 <= data.len() {
                    let v =
                        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0; 4]));
                    out.push_str(&format!("{:X}", v));
                    offset += 4;
                }
            }
            'p' => {
                offset = (offset + 3) & !3;
                if offset + 4 <= data.len() {
                    let v =
                        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0; 4]));
                    out.push_str(&format!("0x{:x}", v));
                    offset += 4;
                }
            }
            's' => {
                let save_offset = offset;
                if let Some(s) = read_len_prefixed_string(data, &mut offset) {
                    out.push_str(&s);
                } else {
                    // Length read failed (out-of-bounds or implausible). Fall back
                    // to reading a NUL-terminated string. EPAT UniLog stores `%s`
                    // bytes directly at the current cursor; older trace payloads
                    // may have an unusable length word before the string.
                    offset = save_offset;
                    offset = (offset + 3) & !3;
                    if !looks_like_inline_c_string(data, offset) {
                        offset += 4;
                    }
                    let start = offset.min(data.len());
                    offset = start;
                    while offset < data.len() && data[offset] != 0 {
                        offset += 1;
                    }
                    let bytes = &data[start..offset];
                    out.push_str(&String::from_utf8_lossy(bytes));
                    // Skip past the NUL if present, then re-align.
                    if offset < data.len() && data[offset] == 0 {
                        offset += 1;
                    }
                    offset = (offset + 3) & !3;
                }
            }
            'c' => {
                offset = (offset + 3) & !3;
                if offset + 4 <= data.len() {
                    let v = data[offset];
                    out.push(v as char);
                    offset += 4;
                }
            }
            'e' if i < chars.len() && chars[i] == '<' => {
                // UniLog enum extension: `%e<EnumType>` or `%e<EnumType , 0xMASK>`.
                // It consumes one u32 argument. EPAT resolves it to `NAME(0xVALUE)`
                // via the per-DB enum tables, but those ship only as an encrypted
                // blob in comdb.txt/reports.txt (decrypted inside EPAT's closed
                // DLL), so we can't map value->name. We still consume the argument
                // — critical, or every specifier after a %e misaligns — and render
                // the enum type plus the raw hex value.
                i += 1; // consume '<'
                let mut type_name = String::new();
                while i < chars.len() && chars[i] != '>' && chars[i] != ',' {
                    type_name.push(chars[i]);
                    i += 1;
                }
                // Skip the optional ", 0xMASK" and the closing '>'.
                while i < chars.len() && chars[i] != '>' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1; // consume '>'
                }
                offset = (offset + 3) & !3;
                if offset + 4 <= data.len() {
                    let v =
                        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0; 4]));
                    offset += 4;
                    let name = type_name.trim();
                    if name.is_empty() {
                        out.push_str(&format!("0x{:x}", v));
                    } else {
                        out.push_str(&format!("{}=0x{:x}", name, v));
                    }
                }
            }
            _ => {
                out.push('%');
                out.push(spec);
            }
        }
    }

    out
}

fn looks_like_inline_c_string(data: &[u8], offset: usize) -> bool {
    let Some(first) = data.get(offset) else {
        return false;
    };
    first.is_ascii_graphic() || *first == b' '
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_d_substitutes_signed_32() {
        let data = (-7i32).to_le_bytes();
        assert_eq!(fmt_printf("x=%d", &data), "x=-7");
    }

    #[test]
    fn percent_u_and_x_share_word() {
        let mut data = Vec::new();
        data.extend_from_slice(&42u32.to_le_bytes());
        data.extend_from_slice(&0xdeadbeefu32.to_le_bytes());
        assert_eq!(fmt_printf("%u/%x", &data), "42/deadbeef");
    }

    #[test]
    fn double_percent_emits_literal() {
        assert_eq!(fmt_printf("100%%", &[]), "100%");
    }

    #[test]
    fn percent_dot_star_s_consumes_len_then_bytes() {
        let mut data = Vec::new();
        data.extend_from_slice(&3u32.to_le_bytes());
        data.extend_from_slice(b"abc");
        // %.*s reads precision from arg stream, then `precision` bytes inline
        assert_eq!(fmt_printf("[%.*s]", &data), "[abc]");
    }

    #[test]
    fn percent_s_reads_length_prefixed_string() {
        // %s consumes a 4-byte length followed by the bytes, then aligns to 4.
        let mut data = Vec::new();
        data.extend_from_slice(&5u32.to_le_bytes());
        data.extend_from_slice(b"hello");
        data.extend_from_slice(&[0; 3]); // pad to 4-byte alignment
        assert_eq!(fmt_printf("[%s]", &data), "[hello]");
    }

    #[test]
    fn percent_p_emits_hex_with_0x_prefix() {
        let data = 0xdeadbeefu32.to_le_bytes();
        assert_eq!(fmt_printf("addr=%p", &data), "addr=0xdeadbeef");
    }

    #[test]
    fn percent_c_takes_low_byte_of_word() {
        let mut data = Vec::new();
        data.extend_from_slice(&[b'A', 0, 0, 0]); // 'A' in low byte of 32-bit slot
        assert_eq!(fmt_printf("ch=%c", &data), "ch=A");
    }

    #[test]
    fn percent_llu_reads_eight_bytes() {
        let v: u64 = 0x0000_0001_0000_0002;
        let data = v.to_le_bytes();
        assert_eq!(fmt_printf("v=%llu", &data), "v=4294967298");
    }

    #[test]
    fn percent_lld_reads_eight_bytes_signed() {
        let v: i64 = -42;
        let data = v.to_le_bytes();
        assert_eq!(fmt_printf("v=%lld", &data), "v=-42");
    }

    #[test]
    fn percent_s_falls_back_to_nul_terminated_if_length_implausible() {
        // First 4 bytes look like a length (0xFFFFFFFF — way too big).
        // Parser should fall back to reading up to NUL.
        let mut data = Vec::new();
        data.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        data.extend_from_slice(b"oops");
        data.push(0);
        assert_eq!(fmt_printf("[%s]", &data), "[oops]");
    }

    #[test]
    fn percent_llx_reads_eight_bytes_hex() {
        let v: u64 = 0xdead_beef_cafe_babe;
        let data = v.to_le_bytes();
        assert_eq!(fmt_printf("v=%llx", &data), "v=deadbeefcafebabe");
    }

    #[test]
    fn percent_ll_x_reads_eight_bytes_hex_upper() {
        let v: u64 = 0xdead_beef_cafe_babe;
        let data = v.to_le_bytes();
        assert_eq!(fmt_printf("v=%llX", &data), "v=DEADBEEFCAFEBABE");
    }

    #[test]
    fn percent_s_does_not_panic_on_short_payload() {
        // Only 2 bytes — too short for either a length prefix or the fallback's
        // post-skip start. Must not panic; must produce empty string for %s.
        let data = [0xAAu8, 0xBB];
        let out = fmt_printf("[%s]", &data);
        assert_eq!(out, "[]");
    }

    #[test]
    fn percent_s_fallback_with_no_nul_terminator_reads_to_end() {
        // length 0xFFFFFFFF, then "abc", no trailing NUL.
        let mut data = Vec::new();
        data.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        data.extend_from_slice(b"abc");
        assert_eq!(fmt_printf("[%s]", &data), "[abc]");
    }

    #[test]
    fn percent_e_enum_renders_type_and_consumes_one_word() {
        let data = 0x7c80_0010u32.to_le_bytes();
        assert_eq!(
            fmt_printf("msg : %e<CcioChanMessageId_e>", &data),
            "msg : CcioChanMessageId_e=0x7c800010"
        );
    }

    #[test]
    fn percent_e_enum_with_mask_consumes_one_word() {
        // The mask is a literal in the format string, not a runtime arg.
        let data = 0x7c80_0010u32.to_le_bytes();
        assert_eq!(
            fmt_printf("msg : %e<CcioChanMessageId_e , 0xFF00FFFF>", &data),
            "msg : CcioChanMessageId_e=0x7c800010"
        );
    }

    #[test]
    fn percent_e_keeps_following_args_aligned() {
        // Regression: %e must consume its u32 so the trailing %x reads the
        // *second* word, not the first.
        let mut data = Vec::new();
        data.extend_from_slice(&0x1111_1111u32.to_le_bytes()); // the enum arg
        data.extend_from_slice(&0xdead_beefu32.to_le_bytes()); // the %x arg
        assert_eq!(
            fmt_printf("%e<FooEnum> x=0x%x", &data),
            "FooEnum=0x11111111 x=0xdeadbeef"
        );
    }

    #[test]
    fn percent_s_falls_back_to_nul_terminated_at_current_offset_for_epat_payload() {
        let data = b"hello rust 3525 uptime=3525208ms\0%um";
        assert_eq!(
            fmt_printf("[%s]", data),
            "[hello rust 3525 uptime=3525208ms]"
        );
    }
}
