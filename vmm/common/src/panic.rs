/*
Copyright 2026 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::{fs::OpenOptions, io::Write};

use backtrace::Backtrace;
use tracing::error;

const KMESG_DEVICE: &str = "/dev/kmsg";

pub fn set_panic_hook() {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let (filename, line) = panic_info
            .location()
            .map(|loc| (loc.file(), loc.line()))
            .unwrap_or(("<unknown>", 0));

        let cause = panic_info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic_info.payload().downcast_ref::<String>().map(|s| &**s))
            .unwrap_or("<unknown>");

        let bt = Backtrace::new();
        let bt_data = format!("{:?}", bt);

        error!(
            "A panic occurred at {}:{}: {}\n{}",
            filename, line, cause, bt_data
        );

        // Explicitly flush to avoid losing logs on abort()
        let _ = std::io::stderr().flush();

        // print panic log to dmesg
        // The panic log size is too large to /dev/kmsg, so write by line.
        match OpenOptions::new().append(true).open(KMESG_DEVICE) {
            Ok(mut kmsg) => {
                let _ = writeln!(kmsg, "A panic occurred at {}:{}: {}", filename, line, cause);
                for bt_line in bt_data.lines() {
                    let _ = writeln!(kmsg, "{}", bt_line);
                }
                let _ = kmsg.flush();
            }
            Err(e) => {
                error!("failed to open {}: {}", KMESG_DEVICE, e);
            }
        }

        prev_hook(panic_info);

        if cfg!(not(debug_assertions)) {
            std::process::abort();
        }
    }));
}
