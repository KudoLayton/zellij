use crate::{os_input_output::AsyncReader, screen::ScreenInstruction, thread_bus::ThreadSenders};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::task;
use zellij_utils::{
    errors::{get_current_ctx, prelude::*, ContextType},
    logging::debug_to_file,
};

pub(crate) struct TerminalBytes {
    terminal_id: u32,
    senders: ThreadSenders,
    async_reader: Box<dyn AsyncReader>,
    debug: bool,
    activity_flag: Arc<AtomicBool>,
    stream_guard: TerminalStreamGuard,
    output_sequence: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalStreamGuard {
    current_generation: Arc<AtomicU64>,
    generation: u64,
}

impl TerminalStreamGuard {
    pub(crate) fn new(current_generation: Arc<AtomicU64>) -> Self {
        let generation = current_generation.load(Ordering::Relaxed);
        Self {
            current_generation,
            generation,
        }
    }

    pub(crate) fn next(current_generation: Arc<AtomicU64>) -> Self {
        let generation = current_generation.fetch_add(1, Ordering::AcqRel) + 1;
        Self {
            current_generation,
            generation,
        }
    }

    pub(crate) fn invalidate(&self) {
        self.current_generation.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    fn is_current(&self) -> bool {
        self.current_generation.load(Ordering::Acquire) == self.generation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutput {
    pub(crate) terminal_id: u32,
    pub(crate) bytes: Vec<u8>,
    pub(crate) generation: Option<u64>,
    pub(crate) sequence: Option<u64>,
}

impl TerminalOutput {
    pub(crate) fn guarded(
        terminal_id: u32,
        bytes: Vec<u8>,
        generation: u64,
        sequence: u64,
    ) -> Self {
        Self {
            terminal_id,
            bytes,
            generation: Some(generation),
            sequence: Some(sequence),
        }
    }

    pub(crate) fn unguarded(terminal_id: u32, bytes: Vec<u8>) -> Self {
        Self {
            terminal_id,
            bytes,
            generation: None,
            sequence: None,
        }
    }
}

impl TerminalBytes {
    pub fn new_with_stream_guard(
        terminal_id: u32,
        async_reader: Box<dyn AsyncReader>,
        senders: ThreadSenders,
        debug: bool,
        activity_flag: Arc<AtomicBool>,
        stream_guard: TerminalStreamGuard,
    ) -> Self {
        TerminalBytes {
            terminal_id,
            senders,
            debug,
            async_reader,
            activity_flag,
            stream_guard,
            output_sequence: 0,
        }
    }

    pub async fn listen(&mut self) -> Result<()> {
        // This function reads bytes from the pty and then sends them as
        // ScreenInstruction::PtyBytes to screen to be parsed there
        // We also send a separate instruction to Screen to render as ScreenInstruction::Render
        //
        // We endeavour to send a Render instruction to screen immediately after having send bytes
        // to parse - this is so that the rendering is quick and smooth. However, this can cause
        // latency if the screen is backed up. For this reason, if we detect a peak in the time it
        // takes to send the render instruction, we assume the screen thread is backed up and so
        // only send a render instruction sparingly, giving screen time to process bytes and render
        // while still allowing the user to see an indication that things are happening (the
        // sparing render instructions)
        let err_context = || "failed to listen for bytes from PTY".to_string();

        let mut err_ctx = get_current_ctx();
        err_ctx.add_call(ContextType::AsyncTask);
        let mut buf = [0u8; 65536];
        loop {
            match self.async_reader.read(&mut buf).await {
                Ok(0) => break, // EOF
                Err(err) => {
                    log::error!("{}", err);
                    break;
                },
                Ok(n_bytes) => {
                    if !self.stream_guard.is_current() {
                        break;
                    }
                    self.activity_flag.store(true, Ordering::Relaxed);
                    let bytes = &buf[..n_bytes];
                    if self.debug {
                        let _ = debug_to_file(bytes, self.terminal_id as i32);
                    }
                    let output = TerminalOutput::guarded(
                        self.terminal_id,
                        bytes.to_vec(),
                        self.stream_guard.generation(),
                        self.output_sequence,
                    );
                    self.output_sequence = self.output_sequence.saturating_add(1);
                    self.async_send_to_screen(ScreenInstruction::PtyBytes(output))
                        .await
                        .with_context(err_context)?;
                },
            }
        }

        // Ignore any errors that happen here.
        // We only leave the loop above when the pane exits. This can happen in a lot of ways, but
        // the most problematic is when quitting zellij with `Ctrl+q`. That is because the channel
        // for `Screen` will have exited already, so this send *will* fail. This isn't a problem
        // per-se because the application terminates anyway, but it will print a lengthy error
        // message into the log for every pane that was still active when we quit the application.
        // This:
        //
        // 1. Makes the log rather pointless, because even when the application exits "normally",
        //    there will be errors inside and
        // 2. Leaves the impression we have a bug in the code and can't terminate properly
        //
        // FIXME: Ideally we detect whether the application is being quit and only ignore the error
        // in that particular case?
        if self.stream_guard.is_current() {
            let _ = self.async_send_to_screen(ScreenInstruction::Render).await;
        }

        Ok(())
    }
    async fn async_send_to_screen(
        &self,
        screen_instruction: ScreenInstruction,
    ) -> Result<Duration> {
        // returns the time it blocked the thread for
        let sent_at = Instant::now();
        let senders = self.senders.clone();
        task::spawn_blocking(move || senders.send_to_screen(screen_instruction))
            .await
            .context("failed to async-send to screen")?
            .context("failed to block on sending message to screen")?;
        Ok(sent_at.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_stream_guard_rejects_stale_generation() {
        let current_generation = Arc::new(AtomicU64::new(0));
        let old_guard = TerminalStreamGuard::new(Arc::clone(&current_generation));
        let fresh_guard = TerminalStreamGuard::next(Arc::clone(&current_generation));

        assert!(!old_guard.is_current());
        assert!(fresh_guard.is_current());

        fresh_guard.invalidate();

        assert!(!fresh_guard.is_current());
    }

    #[test]
    fn guarded_terminal_output_carries_generation_and_sequence() {
        let output = TerminalOutput::guarded(1, b"hello".to_vec(), 2, 3);

        assert_eq!(output.terminal_id, 1);
        assert_eq!(output.bytes, b"hello");
        assert_eq!(output.generation, Some(2));
        assert_eq!(output.sequence, Some(3));
    }

    #[test]
    fn unguarded_terminal_output_has_no_generation_or_sequence() {
        let output = TerminalOutput::unguarded(1, b"internal".to_vec());

        assert_eq!(output.generation, None);
        assert_eq!(output.sequence, None);
    }
}
