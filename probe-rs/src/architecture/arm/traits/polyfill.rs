//! Implementation of the ARM Debug Interface for bit-banging SWD and JTAG probes.
//!
//! This module implements functions to work with chips implementing the ARM Debug version v5.
//!
//! See <https://developer.arm.com/documentation/ihi0031/f/?lang=en> for the ADIv5 specification.

use bitvec::{bitvec, field::BitField, slice::BitSlice, vec::BitVec};

use crate::{
    Error,
    architecture::arm::{
        ArmError, DapError, FullyQualifiedApAddress, RawDapAccess, RegisterAddress,
        ap::AccessPortError,
        dp::{Abort, Ctrl, DPIDR, DebugPortError, DpRegister, RdBuff},
    },
    probe::{
        CommandQueue, CommandResult, DebugProbe, DebugProbeError, IoSequenceItem, JtagAccess,
        JtagSequence, JtagWriteCommand, RawSwdIo, WireProtocol, common::bits_to_byte,
    },
};

const CTRL_PORT: RegisterAddress = RegisterAddress::DpRegister(Ctrl::ADDRESS);

// Constant to be written to ABORT
const JTAG_ABORT_VALUE: u64 = 0x8;

// IR values for JTAG registers
const JTAG_ABORT_IR_VALUE: u32 = 0x8; // A DAP abort, compatible with DPv0
const JTAG_DEBUG_PORT_IR_VALUE: u32 = 0xA;
const JTAG_ACCESS_PORT_IR_VALUE: u32 = 0xB;

const JTAG_STATUS_WAIT: u32 = 0x1;
/// OK/FAULT response
const JTAG_STATUS_OK: u32 = 0x2;

// ARM DR accesses are always 35 bits wide
const JTAG_DR_BIT_LENGTH: u32 = 35;

// Build a JTAG payload
fn build_jtag_payload_and_address(transfer: &DapTransfer) -> (u64, u32) {
    if transfer.is_abort() {
        (JTAG_ABORT_VALUE, JTAG_ABORT_IR_VALUE)
    } else {
        let address = match transfer.address.is_ap() {
            false => JTAG_DEBUG_PORT_IR_VALUE,
            true => JTAG_ACCESS_PORT_IR_VALUE,
        };

        let port_address = transfer.address.a2_and_3();
        let mut payload = 0u64;

        // 32-bit value, bits 35:3
        payload |= (transfer.value as u64) << 3;
        // A[3:2], bits 2:1
        payload |= (port_address as u64 & 0b1000) >> 1;
        payload |= (port_address as u64 & 0b0100) >> 1;
        // RnW, bit 0
        payload |= u64::from(transfer.direction == TransferDirection::Read);

        (payload, address)
    }
}

fn parse_jtag_response(data: &BitSlice) -> u64 {
    data.load_le::<u64>()
}

/// Perform a single JTAG transfer and parse the results
///
/// Return is (value, status)
fn perform_jtag_transfer<P: JtagAccess + RawSwdIo>(
    probe: &mut P,
    transfer: &DapTransfer,
) -> Result<(u32, TransferStatus), DebugProbeError> {
    // Determine what JTAG IR address and value to send
    let (payload, address) = build_jtag_payload_and_address(transfer);
    let data = payload.to_le_bytes();

    let idle_cycles = probe.idle_cycles();
    probe.set_idle_cycles(transfer.idle_cycles_after.min(255) as u8)?;

    // This is a bit confusing, but a read from any port is still
    // a JTAG write as we have to transmit the address
    let result = probe.write_register(address, &data[..], JTAG_DR_BIT_LENGTH);

    probe.set_idle_cycles(idle_cycles)?;

    let result = result?;

    let received = parse_jtag_response(&result);

    if transfer.is_abort() {
        // No responses returned from this
        return Ok((0, TransferStatus::Ok));
    }

    // Received value is bits [35:3]
    let received_value = (received >> 3) as u32;
    // Status is bits [2:0]
    let status = (received & 0b111) as u32;

    let transfer_status = match status {
        s if s == JTAG_STATUS_WAIT => TransferStatus::Failed(DapError::WaitResponse),
        s if s == JTAG_STATUS_OK => TransferStatus::Ok,
        _ => {
            tracing::debug!("Unexpected DAP response: {}", status);

            TransferStatus::Failed(DapError::NoAcknowledge)
        }
    };

    Ok((received_value, transfer_status))
}

/// Perform a batch of JTAG transfers.
///
/// Each transfer is sent one at a time using the JtagAccess trait
fn perform_jtag_transfers<P: JtagAccess + RawSwdIo>(
    probe: &mut P,
    transfers: &mut [DapTransfer],
) -> Result<(), DebugProbeError> {
    // Set up the command queue.
    let mut queue = CommandQueue::new();

    let mut results = vec![];

    for transfer in transfers.iter() {
        results.push(queue.schedule(transfer.jtag_write()));
    }

    let last_is_abort = transfers[transfers.len() - 1].is_abort();
    let last_is_rdbuff = transfers[transfers.len() - 1].is_rdbuff();
    if !last_is_abort && !last_is_rdbuff {
        // Need to issue a fake read to get final ack
        results.push(queue.schedule(DapTransfer::read(RdBuff::ADDRESS).jtag_write()));
    }

    if !last_is_abort {
        // Check CTRL/STATUS to make sure OK/FAULT meant OK
        results.push(queue.schedule(DapTransfer::read(Ctrl::ADDRESS).jtag_write()));
        results.push(queue.schedule(DapTransfer::read(RdBuff::ADDRESS).jtag_write()));
    }

    let mut status_responses = vec![TransferStatus::Pending; results.len()];

    // Simplification: use the maximum idle cycles of all transfers, because the batched API
    // doesn't allow for individual values.
    let max_idle_cycles = transfers
        .iter()
        .map(|t| t.idle_cycles_after)
        .max()
        .unwrap_or(0);
    let idle_cycles = probe.idle_cycles();
    probe.set_idle_cycles(max_idle_cycles.min(255) as u8)?;

    // Execute as much of the queue as we can. We'll handle the rest in a following iteration
    // if we can.
    let mut jtag_results;
    match probe.write_register_batch(&queue) {
        Ok(r) => {
            status_responses.fill(TransferStatus::Ok);
            jtag_results = r;
        }
        Err(e) => {
            let current_idx = e.results.len();
            status_responses[..current_idx].fill(TransferStatus::Ok);
            jtag_results = e.results;

            match e.error {
                Error::Arm(ArmError::AccessPort {
                    address: _,
                    source: AccessPortError::DebugPort(DebugPortError::Dap(failure)),
                }) => {
                    // Mark all subsequent transactions with the same failure.
                    status_responses[current_idx..].fill(TransferStatus::Failed(failure));
                    jtag_results.push(&results[current_idx], CommandResult::None);
                }
                Error::Probe(error) => return Err(error),
                _other => unreachable!(),
            }
        }
    }

    probe.set_idle_cycles(idle_cycles)?;

    // Process the results. At this point we should only have OK/FAULT responses.
    for (i, transfer) in transfers.iter_mut().enumerate() {
        transfer.status = *status_responses.get(i + 1).unwrap_or(&TransferStatus::Ok);
    }

    // Pluck off the extra 2 results that do error checking
    let ctrl_value = if !last_is_abort {
        _ = results
            .pop()
            .expect("Failed to pop value that was pushed here.");
        let rdbuff_result = results
            .pop()
            .expect("Failed to pop value that was pushed here.");

        Some(rdbuff_result)
    } else {
        None
    };

    // Shift the results.
    // Each response is read in the next transaction, so skip 1
    for (i, result) in results.into_iter().skip(1).enumerate() {
        let transfer = &mut transfers[i];
        if transfer.is_abort() || transfer.is_rdbuff() {
            transfer.status = TransferStatus::Ok;
            continue;
        }

        if transfer.status == TransferStatus::Ok && transfer.direction == TransferDirection::Read {
            let response = jtag_results.take(result).unwrap();
            transfer.value = response.into_u32();
        }
    }

    if let Some(ctrl_value) = ctrl_value {
        // Check CTRL/STATUS to make sure OK/FAULT meant OK
        if let Ok(CommandResult::U32(received_value)) = jtag_results.take(ctrl_value) {
            if Ctrl(received_value).sticky_err() {
                tracing::debug!("JTAG transaction set failed: {:#X?}", transfers);

                // Clear the sticky bit so future transactions succeed
                let (_, _) = perform_jtag_transfer(
                    probe,
                    &DapTransfer::write(Ctrl::ADDRESS, received_value),
                )?;

                // Mark OK/FAULT transactions as failed. Since the error is sticky, we can assume that
                // if we received a WAIT, the previous transactions were successful.
                // The caller will reset the sticky flag and retry if needed
                for transfer in transfers.iter_mut() {
                    if transfer.status == TransferStatus::Ok {
                        transfer.status = TransferStatus::Failed(DapError::FaultResponse);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Perform a batch of SWD transfers.
///
/// For each transfer, the corresponding bit sequence is
/// created and the resulting sequences are concatenated
/// to a single sequence, so that it can be sent to
/// to the probe.
fn perform_swd_transfers<P: RawSwdIo>(
    probe: &mut P,
    transfers: &mut [DapTransfer],
) -> Result<(), DebugProbeError> {
    let mut io_sequence = IoSequence::new();

    for transfer in transfers.iter() {
        io_sequence.extend(&transfer.io_sequence());
    }

    let result = probe.swd_io(io_sequence.io_items())?;

    let mut result_bits = &result[..];

    for (i, transfer) in transfers.iter_mut().enumerate() {
        // There are eight request bits, the response comes directly after.
        let response_offset = 8;
        let response = parse_swd_response(&result_bits[response_offset..], transfer.direction);

        probe.probe_statistics().report_swd_response(&response);

        transfer.status = match response {
            Ok(response) => {
                transfer.value = response;
                TransferStatus::Ok
            }

            Err(e) => TransferStatus::Failed(e),
        };

        tracing::trace!(
            "Transfer result {}: {:?} {:x?}",
            i,
            transfer.status,
            transfer.value
        );

        result_bits = &result_bits[transfer.swd_response_length()..];
    }

    Ok(())
}

/// Perform a batch of transfers.
///
/// Certain transfers require additional transfers to
/// get the result. This is handled by this function.
///
/// Retries on WAIT responses are automatically handled.
///
/// Other errors are not handled, so the debug interface might be in an error state
/// after this function returns.
fn perform_transfers<P: DebugProbe + RawSwdIo + JtagAccess>(
    probe: &mut P,
    transfers: &mut [DapTransfer],
) -> Result<(), ArmError> {
    assert!(!transfers.is_empty());

    // Read from DebugPort  -> Nothing special needed
    // Read from AccessPort -> Response is returned in next read
    //                         -> The next transfer must be a AP Read, otherwise we need to insert a read from the RDBUFF register
    // Write to any port    -> Status is reported in next transfer
    // Write to any port    -> Writes can be buffered, so certain transfers have to be avoided until a instruction which can be stalled is performed

    let mut final_transfers: Vec<DapTransfer> = Vec::with_capacity(transfers.len());

    struct OriginalTransfer {
        index: usize,
        response_in_next: bool,
    }
    let mut result_indices = Vec::with_capacity(transfers.len());

    let wire_protocol = probe.active_protocol().unwrap();

    for (i, transfer) in transfers.iter().enumerate() {
        // The response for an AP read is returned in the next response
        let need_ap_read = transfer.is_ap_read();

        // Writes to the AP can be buffered
        //
        // TODO: Can DP writes be buffered as well?
        let buffered_write = transfer.is_ap_write();

        // For all writes, except writes to the DP ABORT register, we need to perform another register to ensure that
        // we know if the write succeeded.
        let write_response_pending = transfer.is_write() && !transfer.is_abort();

        // Track whether the response is returned in the next transfer.
        // SWD only, with JTAG we always get responses in a predictable fashion so it's
        // handled by perform_jtag_transfers
        result_indices.push(OriginalTransfer {
            index: final_transfers.len(),
            response_in_next: wire_protocol == WireProtocol::Swd
                && (need_ap_read || write_response_pending),
        });
        let transfer = if transfer.is_write() {
            let mut transfer = transfer.clone();
            transfer.idle_cycles_after = probe.swd_settings().num_idle_cycles_between_writes;
            transfer
        } else {
            transfer.clone()
        };
        final_transfers.push(transfer);

        if wire_protocol == WireProtocol::Jtag {
            continue;
        }

        // Now process the extra transfers needed
        let mut extra_idle_cycles = probe.swd_settings().idle_cycles_before_write_verify;
        let mut need_extra = false;
        if let Some(next) = transfers.get(i + 1) {
            // Check if we need to insert an additional read from the RDBUFF register
            if need_ap_read && !next.is_ap_read() {
                need_extra = true;
                extra_idle_cycles = 0;
            } else if buffered_write && next.must_not_stall() {
                // We need an additional instruction to avoid losing buffered writes.
                need_extra = true;
            } else {
                // No extra transfer needed
            }
        } else {
            // Last transfer
            if !write_response_pending {
                extra_idle_cycles = 0;
            }

            // We need an additional instruction to avoid losing writes or returned values.
            if need_ap_read || write_response_pending {
                need_extra = true;
            }
        };

        if need_extra {
            final_transfers.last_mut().unwrap().idle_cycles_after += extra_idle_cycles;

            // Add a read from RDBUFF, this access will be stalled by the DebugPort if the write
            // buffer is not empty.
            // This is an extra transfer, which doesn't have a reponse on it's own.
            final_transfers.push(DapTransfer::read(RdBuff::ADDRESS));
            probe.probe_statistics().record_extra_transfer();
        }
    }

    // Add idle cycles at the end, to ensure transfer is performed
    final_transfers.last_mut().unwrap().idle_cycles_after +=
        probe.swd_settings().idle_cycles_after_transfer;

    let num_transfers = final_transfers.len();
    tracing::debug!(
        "Performing {} transfers ({} additional transfers)",
        num_transfers,
        num_transfers - transfers.len()
    );

    probe.probe_statistics().record_transfers(num_transfers);

    perform_raw_transfers_retry(probe, &mut final_transfers)?;

    // Retrieve the results
    for (transfer, orig) in transfers.iter_mut().zip(result_indices) {
        // if the original transfer caused two transfers, return the first non-OK status.
        // This is important if the first fails with WAIT and the second with FAULT. We need to
        // return WAIT so that higher layers know they have to retry.
        transfer.status = final_transfers[orig.index].status;

        let response_idx = orig.index + orig.response_in_next as usize;
        if orig.response_in_next && transfer.status == TransferStatus::Ok {
            transfer.status = final_transfers[response_idx].status;
        }

        if transfer.direction == TransferDirection::Read {
            transfer.value = final_transfers[response_idx].value;
        }
    }

    Ok(())
}

/// Perform a batch of raw transfers, retrying on WAIT responses.
///
/// Other than that, the transfers are sent as-is. You might want to use `perform_transfers` instead, which
/// does correction for delayed FAULT responses and other helpful stuff.
fn perform_raw_transfers_retry<P: DebugProbe + RawSwdIo + JtagAccess>(
    probe: &mut P,
    transfers: &mut [DapTransfer],
) -> Result<(), ArmError> {
    let mut successful_transfers = 0;
    let mut idle_cycles = std::cmp::max(1, probe.swd_settings().num_idle_cycles_between_writes);

    let num_retries = probe.swd_settings().num_retries_after_wait;

    'transfer: for _ in 0..num_retries {
        let chunk = &mut transfers[successful_transfers..];
        assert!(!chunk.is_empty());

        perform_raw_transfers(probe, chunk)?;

        for transfer in chunk.iter() {
            match transfer.status {
                TransferStatus::Ok => successful_transfers += 1,
                TransferStatus::Failed(DapError::WaitResponse) => {
                    tracing::debug!("got WAIT on transfer {}, retrying...", successful_transfers);

                    // Surface this error, because it indicates there's a low-level protocol problem going on.
                    clear_overrun_and_sticky_err(probe).inspect_err(|e| {
                        tracing::error!("error clearing sticky overrun/error bits: {e}");
                    })?;

                    // Increase idle cycles of the failed write transfer and the rest of the chunk
                    for transfer in &mut chunk[..] {
                        if transfer.is_write() {
                            transfer.idle_cycles_after += idle_cycles;
                        }
                    }
                    idle_cycles = std::cmp::min(
                        probe.swd_settings().max_retry_idle_cycles_after_wait,
                        2 * idle_cycles,
                    );

                    continue 'transfer;
                }
                status => {
                    tracing::debug!(
                        "Transfer {}/{} failed: {:?}",
                        successful_transfers + 1,
                        transfers.len(),
                        status
                    );

                    return Ok(());
                }
            }
        }

        if successful_transfers == transfers.len() {
            return Ok(());
        }
    }

    // Timeout, abort transactions
    tracing::debug!(
        "Timeout in SWD transaction, aborting AP transactions after {num_retries} retries."
    );
    write_dp_register(probe, {
        let mut abort = Abort(0);
        abort.set_dapabort(true);
        abort
    })?;

    // Need to return Ok here, the caller will handle errors in transfer status.
    Ok(())
}

fn clear_overrun_and_sticky_err<P: DebugProbe + RawSwdIo + JtagAccess>(
    probe: &mut P,
) -> Result<(), ArmError> {
    tracing::debug!("Clearing overrun and sticky error");
    // Build ABORT transfer.
    write_dp_register(probe, {
        let mut abort = Abort(0);
        abort.set_orunerrclr(true);
        abort.set_stkerrclr(true);
        abort
    })
}

fn write_dp_register<P: DebugProbe + RawSwdIo + JtagAccess, R: DpRegister>(
    probe: &mut P,
    register: R,
) -> Result<(), ArmError> {
    let mut transfer = DapTransfer::write(R::ADDRESS, register.into());

    transfer.idle_cycles_after = probe.swd_settings().idle_cycles_before_write_verify
        + probe.swd_settings().num_idle_cycles_between_writes;

    // Do it
    perform_raw_transfers(probe, std::slice::from_mut(&mut transfer))?;

    if let TransferStatus::Failed(e) = transfer.status {
        Err(e)?
    }

    Ok(())
}

/// Perform a batch of raw transfers.
///
/// This function will just send the transfers as-is, without handling WAIT or FAULT response.
/// See [`perform_raw_transfers_retry`] for a version that handles WAIT responses
fn perform_raw_transfers<P: DebugProbe + RawSwdIo + JtagAccess>(
    probe: &mut P,
    transfers: &mut [DapTransfer],
) -> Result<(), DebugProbeError> {
    match probe.active_protocol().unwrap() {
        WireProtocol::Swd => perform_swd_transfers(probe, transfers),
        WireProtocol::Jtag => perform_jtag_transfers(probe, transfers),
    }
}

#[derive(Debug, Clone)]
struct DapTransfer {
    address: RegisterAddress,
    direction: TransferDirection,
    value: u32,
    status: TransferStatus,
    idle_cycles_after: usize,
}

impl DapTransfer {
    fn read<P: Into<RegisterAddress>>(address: P) -> DapTransfer {
        Self {
            address: address.into(),
            direction: TransferDirection::Read,
            value: 0,
            status: TransferStatus::Pending,
            idle_cycles_after: 0,
        }
    }

    fn write<P: Into<RegisterAddress>>(address: P, value: u32) -> DapTransfer {
        Self {
            address: address.into(),
            value,
            direction: TransferDirection::Write,
            status: TransferStatus::Pending,
            idle_cycles_after: 0,
        }
    }

    fn transfer_type(&self) -> TransferType {
        match self.direction {
            TransferDirection::Read => TransferType::Read,
            TransferDirection::Write => TransferType::Write(self.value),
        }
    }

    fn io_sequence(&self) -> IoSequence {
        let mut seq = build_swd_transfer(&self.address, self.transfer_type());

        seq.reserve(self.idle_cycles_after);
        for _ in 0..self.idle_cycles_after {
            seq.add_output(false);
        }

        seq
    }

    fn jtag_write(&self) -> JtagWriteCommand {
        let (payload, address) = if self.is_abort() {
            (JTAG_ABORT_VALUE, JTAG_ABORT_IR_VALUE)
        } else {
            let jtag_address = match self.address.is_ap() {
                false => JTAG_DEBUG_PORT_IR_VALUE,
                true => JTAG_ACCESS_PORT_IR_VALUE,
            };
            let port_address = self.address.a2_and_3();

            let mut payload = 0u64;

            // 32-bit value, bits 35:3
            payload |= (self.value as u64) << 3;
            // A[3:2], bits 2:1
            payload |= (port_address as u64 & 0b1000) >> 1;
            payload |= (port_address as u64 & 0b0100) >> 1;
            // RnW, bit 0
            payload |= u64::from(self.direction == TransferDirection::Read);

            (payload, jtag_address)
        };

        JtagWriteCommand {
            address,
            data: payload.to_le_bytes().to_vec(),
            len: JTAG_DR_BIT_LENGTH,
            transform: |command, response| {
                // No responses returned for aborts.
                if command.address == JTAG_ABORT_IR_VALUE {
                    return Ok(CommandResult::None);
                }

                let received = parse_jtag_response(response);

                // Received value is bits [35:3]
                let received_value = (received >> 3) as u32;
                // Status is bits [2:0]
                let status = (received & 0b111) as u32;

                let error = match status {
                    s if s == JTAG_STATUS_OK => return Ok(CommandResult::U32(received_value)),
                    s if s == JTAG_STATUS_WAIT => DapError::WaitResponse,
                    _ => {
                        tracing::debug!("Unexpected DAP response: {}", status);

                        DapError::NoAcknowledge
                    }
                };

                Err(Error::Arm(ArmError::AccessPort {
                    address: FullyQualifiedApAddress::v1_with_default_dp(0), // Dummy value, unused
                    source: AccessPortError::DebugPort(DebugPortError::Dap(error)),
                }))
            },
        }
    }

    // Helper functions for combining transfers

    fn is_ap_read(&self) -> bool {
        self.address.is_ap() && self.direction == TransferDirection::Read
    }

    fn is_ap_write(&self) -> bool {
        self.address.is_ap() && self.direction == TransferDirection::Write
    }

    fn is_write(&self) -> bool {
        self.direction == TransferDirection::Write
    }

    fn is_abort(&self) -> bool {
        matches!(self.address, RegisterAddress::DpRegister(Abort::ADDRESS))
            && self.direction == TransferDirection::Write
    }

    fn is_rdbuff(&self) -> bool {
        matches!(self.address, RegisterAddress::DpRegister(RdBuff::ADDRESS))
            && self.direction == TransferDirection::Read
    }

    fn swd_response_length(&self) -> usize {
        self.direction.swd_response_length() + self.idle_cycles_after
    }

    fn must_not_stall(&self) -> bool {
        // These requests must not issue a WRITE response. This means we need to
        // add an additional read from the RDBUFF register to stall the request until
        // the write buffer is empty.
        let abort_write = self.is_abort();

        let dpidr_read = matches!(self.address, RegisterAddress::DpRegister(DPIDR::ADDRESS))
            && self.direction == TransferDirection::Read;

        let ctrl_stat_read = matches!(self.address, RegisterAddress::DpRegister(Ctrl::ADDRESS))
            && self.direction == TransferDirection::Read;

        abort_write || dpidr_read || ctrl_stat_read
    }
}

#[derive(Debug, PartialEq, Copy, Clone)]
enum TransferDirection {
    Read,
    Write,
}

impl TransferDirection {
    const fn swd_response_length(self) -> usize {
        match self {
            TransferDirection::Read => 8 + 3 + 32 + 1 + 2,
            TransferDirection::Write => 8 + 3 + 2 + 32 + 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TransferStatus {
    Pending,
    /// OK/FAULT response
    Ok,
    Failed(DapError),
}

/// Output only variant of [`IoSequence`]
struct OutSequence {
    bits: Vec<bool>,
}

impl OutSequence {
    fn new() -> Self {
        OutSequence { bits: vec![] }
    }

    fn from_bytes(data: &[u8], mut bits: usize) -> Self {
        let mut this = Self::new();

        'outer: for byte in data {
            for i in 0..8 {
                this.add_output(byte & (1 << i) != 0);
                bits -= 1;
                if bits == 0 {
                    break 'outer;
                }
            }
        }

        this
    }

    fn add_output(&mut self, bit: bool) {
        self.bits.push(bit);
    }

    fn len(&self) -> usize {
        self.bits.len()
    }

    fn bits(&self) -> &[bool] {
        &self.bits
    }

    fn io_items(&self) -> impl Iterator<Item = IoSequenceItem> {
        self.bits.iter().map(|bit| IoSequenceItem::Output(*bit))
    }
}

struct IoSequence {
    io: Vec<IoSequenceItem>,
}

impl IoSequence {
    fn new() -> Self {
        IoSequence { io: vec![] }
    }

    fn with_capacity(capacity: usize) -> Self {
        IoSequence {
            io: Vec::with_capacity(capacity),
        }
    }

    fn reserve(&mut self, idle_cycles_after: usize) {
        self.io.reserve(idle_cycles_after);
    }

    fn add_output(&mut self, bit: bool) {
        self.io.push(IoSequenceItem::Output(bit));
    }

    fn add_input(&mut self) {
        self.io.push(IoSequenceItem::Input);
    }

    fn add_input_sequence(&mut self, length: usize) {
        for _ in 0..length {
            self.add_input();
        }
    }

    fn io_items(&self) -> impl Iterator<Item = IoSequenceItem> + '_ {
        self.io.iter().copied()
    }

    fn extend(&mut self, other: &IoSequence) {
        self.io.extend_from_slice(&other.io);
    }
}

impl From<OutSequence> for IoSequence {
    fn from(out_sequence: OutSequence) -> Self {
        let mut io_sequence = IoSequence::with_capacity(out_sequence.len());

        for bi in out_sequence.bits {
            io_sequence.add_output(bi);
        }

        io_sequence
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum TransferType {
    Read,
    Write(u32),
}

fn build_swd_transfer(address: &RegisterAddress, direction: TransferType) -> IoSequence {
    // JLink operates on raw SWD bit sequences.
    // So we need to manually assemble the read and write bitsequences.
    // The following code with the comments hopefully explains well enough how it works.
    // `true` means `1` and `false` means `0` for the SWDIO sequence.
    // `true` means `drive line` and `false` means `open drain` for the direction sequence.

    // First we determine the APnDP bit.
    let ap_n_dp = address.is_ap();

    // Set direction bit to 1 for reads.
    let direction_bit = direction == TransferType::Read;

    // Then we determine the address bits.
    // Only bits 2 and 3 are relevant as we use byte addressing but can only read 32bits
    // which means we can skip bits 0 and 1. The ADI specification is defined like this.
    let a2 = address.a2();
    let a3 = address.a3();

    let mut sequence = IoSequence::with_capacity(46);

    // Then we assemble the actual request.

    // Start bit (always 1).
    sequence.add_output(true);

    // APnDP (0 for DP, 1 for AP).
    sequence.add_output(ap_n_dp);

    // RnW (0 for Write, 1 for Read).
    sequence.add_output(direction_bit);

    // Address bits
    sequence.add_output(a2);
    sequence.add_output(a3);

    // Odd parity bit over APnDP, RnW a2 and a3
    sequence.add_output(ap_n_dp ^ direction_bit ^ a2 ^ a3);

    // Stop bit (always 0).
    sequence.add_output(false);

    // Park bit (always 1).
    sequence.add_output(true);

    // Turnaround bit.
    sequence.add_input();

    // ACK bits.
    sequence.add_input_sequence(3);

    if let TransferType::Write(value) = direction {
        // For writes, we need to a turnaround bit.
        sequence.add_input();

        // Now we add all the data bits to the sequence.
        for i in 0..32 {
            sequence.add_output(value & (1 << i) != 0);
        }

        // Add the parity of the data bits.
        sequence.add_output(value.count_ones() % 2 == 1);
    } else {
        // Handle Read
        // Add the data bits to the SWDIO sequence.
        sequence.add_input_sequence(32);

        // Add the parity bit to the sequence.
        sequence.add_input();

        // Finally add the turnaround bit to the sequence.
        sequence.add_input();
    }

    sequence
}

/// Parses acknowledgement and extracts the data from the response if the transfer is a Read.
fn parse_swd_response(resp: &[bool], direction: TransferDirection) -> Result<u32, DapError> {
    // We need to discard the output bits that correspond to the part of the request
    // in which the probe is driving SWDIO. Additionally, there is a phase shift that
    // happens when ownership of the SWDIO line is transfered to the device.
    // The device changes the value of SWDIO with the rising edge of the clock.
    //
    // It appears that the JLink probe samples this line with the falling edge of
    // the clock. Therefore, the whole sequence seems to be leading by one bit,
    // which is why we don't discard the turnaround bit. It actually contains the
    // first ack bit.

    let (ack, response) = resp.split_at(3);

    // When all bits are high, this means we didn't get any response from the
    // target, which indicates a protocol error.
    match (ack[0], ack[1], ack[2]) {
        (true, true, true) => Err(DapError::NoAcknowledge),
        (false, true, false) => Err(DapError::WaitResponse),
        (false, false, true) => Err(DapError::FaultResponse),
        // Successful transfer
        (true, false, false) if direction == TransferDirection::Read => {
            // Take the data bits and convert them into a 32bit int.
            let value = bits_to_byte(response.iter().copied());

            // Make sure the parity is correct.
            if value.count_ones() % 2 == response[32] as u32 {
                tracing::trace!("DAP read {}.", value);
                Ok(value)
            } else {
                Err(DapError::IncorrectParity)
            }
        }
        (true, false, false) => Ok(0), // Write; there are no data bits in the mandatory data phase.
        _ => {
            // Invalid response
            tracing::debug!(
                "Unexpected response from target, does not conform to SWD specfication (ack={:?})",
                resp
            );
            Err(DapError::Protocol(WireProtocol::Swd))
        }
    }
}

/// RawDapAccess implementation for probes that implement RawProtocolIo.
// TODO: JTAG shouldn't be required, but an option - maybe via trait downcasting?
impl<Probe: DebugProbe + RawSwdIo + JtagAccess + 'static> RawDapAccess for Probe {
    fn raw_read_register(&mut self, address: RegisterAddress) -> Result<u32, ArmError> {
        let mut transfer = DapTransfer::read(address);
        perform_transfers(self, std::slice::from_mut(&mut transfer))?;

        match transfer.status {
            TransferStatus::Ok => Ok(transfer.value),
            TransferStatus::Failed(DapError::FaultResponse) => {
                tracing::debug!("DAP FAULT");

                // A fault happened during operation.

                // To get a clue about the actual fault we want to read the ctrl register,
                // which will have the fault status flags set. But we only do this
                // if we are *not* currently reading the ctrl register, otherwise
                // this could end up being an endless recursion.

                if address == CTRL_PORT {
                    //  This is not necessarily the CTRL/STAT register, because the dpbanksel field in the SELECT register
                    //  might be set so that the read wasn't actually from the CTRL/STAT register.
                    tracing::debug!(
                        "Read might have been from CTRL/STAT register, not reading it again to dermine fault reason"
                    );

                    // We still clear the sticky error, otherwise all future accesses will fail.
                    //
                    // We also assume that we use overrun detection, so we clear the overrun error as well.
                    clear_overrun_and_sticky_err(self)?;
                } else {
                    // Reading the CTRL/AP register depends on the dpbanksel register, but we don't know
                    // here what the value of it is. So this will fail if dpbanksel is not set to 0,
                    // but there is no way of figuring that out here, because reading the SELECT register
                    // would also fail.
                    //
                    // What might happen is that the read fails, but that would then trigger another fault handling,
                    // so it all ends up working.
                    tracing::debug!("Reading CTRL/AP register to determine reason for FAULT");
                    let response = RawDapAccess::raw_read_register(self, CTRL_PORT)?;
                    let ctrl = Ctrl::try_from(response)?;
                    tracing::debug!(
                        "Reading DAP register failed. Ctrl/Stat register value is: {:#?}",
                        ctrl
                    );

                    // Check the reason for the fault
                    // Other fault reasons than overrun or write error are not handled yet.
                    if ctrl.sticky_orun() || ctrl.sticky_err() {
                        // Clear the error state
                        clear_overrun_and_sticky_err(self)?;
                    }
                }

                Err(DapError::FaultResponse.into())
            }
            // The other errors mean that something went wrong with the protocol itself.
            // There's no guaranteed correct way to recover, so don't.
            TransferStatus::Failed(e) => Err(e.into()),
            other => panic!(
                "Unexpected transfer state after reading register: {other:?}. This is a bug!"
            ),
        }
    }

    fn raw_read_block(
        &mut self,
        address: RegisterAddress,
        values: &mut [u32],
    ) -> Result<(), ArmError> {
        let mut transfers = vec![DapTransfer::read(address); values.len()];

        perform_transfers(self, &mut transfers)?;

        for (i, result) in transfers.iter().enumerate() {
            match result.status {
                TransferStatus::Ok => values[i] = result.value,
                TransferStatus::Failed(err) => {
                    tracing::info!(
                        "Error in access {}/{} of block access: {:?}",
                        i + 1,
                        values.len(),
                        err
                    );

                    // TODO: The error reason could be investigated by reading the CTRL/STAT register here,

                    if err == DapError::FaultResponse {
                        clear_overrun_and_sticky_err(self)?;
                    }

                    return Err(err.into());
                }
                other => panic!(
                    "Unexpected transfer state after reading registers: {other:?}. This is a bug!"
                ),
            }
        }

        Ok(())
    }

    fn raw_write_register(&mut self, address: RegisterAddress, value: u32) -> Result<(), ArmError> {
        let mut transfer = DapTransfer::write(address, value);

        perform_transfers(self, std::slice::from_mut(&mut transfer))?;

        match transfer.status {
            TransferStatus::Ok => Ok(()),
            TransferStatus::Failed(DapError::FaultResponse) => {
                tracing::warn!("DAP FAULT");
                // A fault happened during operation.

                // To get a clue about the actual fault we read the ctrl register,
                // which will have the fault status flags set.

                // This read might fail because the dpbanksel register is not set to 0.
                let response = RawDapAccess::raw_read_register(self, CTRL_PORT)?;

                let ctrl = Ctrl::try_from(response)?;
                tracing::warn!(
                    "Writing DAP register failed. Ctrl/Stat register value is: {:#?}",
                    ctrl
                );

                // Check the reason for the fault
                // Other fault reasons than overrun or write error are not handled yet.
                if ctrl.sticky_orun() || ctrl.sticky_err() {
                    // We did not handle a WAIT state properly

                    // Because we use overrun detection, we now have to clear the overrun error
                    clear_overrun_and_sticky_err(self)?;
                }

                Err(DapError::FaultResponse.into())
            }
            // The other errors mean that something went wrong with the protocol itself.
            // There's no guaranteed correct way to recover, so don't.
            TransferStatus::Failed(e) => Err(e.into()),
            other => panic!(
                "Unexpected transfer state after writing register: {other:?}. This is a bug!"
            ),
        }
    }

    fn raw_write_block(
        &mut self,
        address: RegisterAddress,
        values: &[u32],
    ) -> Result<(), ArmError> {
        let mut transfers = values
            .iter()
            .map(|v| DapTransfer::write(address, *v))
            .collect::<Vec<_>>();

        perform_transfers(self, &mut transfers)?;

        for (i, result) in transfers.iter().enumerate() {
            match result.status {
                TransferStatus::Ok => {}
                TransferStatus::Failed(err) => {
                    tracing::debug!(
                        "Error in access {}/{} of block access: {}",
                        i + 1,
                        values.len(),
                        err
                    );

                    // TODO: The error reason could be investigated by reading the CTRL/STAT register here,
                    if err == DapError::FaultResponse {
                        clear_overrun_and_sticky_err(self)?;
                    }

                    return Err(err.into());
                }
                other => panic!(
                    "Unexpected transfer state after writing registers: {other:?}. This is a bug!"
                ),
            }
        }

        Ok(())
    }

    fn swj_pins(
        &mut self,
        pin_out: u32,
        pin_select: u32,
        pin_wait: u32,
    ) -> Result<u32, DebugProbeError> {
        RawSwdIo::swj_pins(self, pin_out, pin_select, pin_wait)
    }

    fn into_probe(self: Box<Self>) -> Box<dyn DebugProbe> {
        self
    }

    fn jtag_sequence(&mut self, bit_len: u8, tms: bool, bits: u64) -> Result<(), DebugProbeError> {
        let mut data = BitVec::with_capacity(bit_len as usize);

        for i in 0..bit_len {
            data.push((bits >> i) & 1 == 1);
        }

        self.shift_raw_sequence(JtagSequence {
            tms,
            data,
            tdo_capture: false,
        })?;

        Ok(())
    }

    fn swj_sequence(&mut self, bit_len: u8, bits: u64) -> Result<(), DebugProbeError> {
        let protocol = self.active_protocol().unwrap();

        let io_sequence = OutSequence::from_bytes(&bits.to_le_bytes(), bit_len as usize);
        send_sequence(self, protocol, &io_sequence)
    }

    fn core_status_notification(&mut self, _: crate::CoreStatus) -> Result<(), DebugProbeError> {
        Ok(())
    }
}

fn send_sequence<P: RawSwdIo + JtagAccess>(
    probe: &mut P,
    protocol: WireProtocol,
    sequence: &OutSequence,
) -> Result<(), DebugProbeError> {
    match protocol {
        WireProtocol::Jtag => {
            // Swj sequences should be shifted out to tms, since that is the pin
            // shared between swd and jtag modes.
            let mut bits = sequence.bits().iter().peekable();
            while let Some(first) = bits.next() {
                let mut count = 1;
                while let Some(next) = bits.peek() {
                    if first != *next {
                        break;
                    }
                    count += 1;
                    bits.next();
                }

                probe.shift_raw_sequence(JtagSequence {
                    tms: *first,
                    data: bitvec![0; count],
                    tdo_capture: false,
                })?;
            }
        }
        WireProtocol::Swd => {
            probe.swd_io(sequence.io_items())?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use crate::{
        architecture::arm::{
            ApAddress, RawDapAccess, RegisterAddress,
            dp::{Ctrl, DpRegister, RdBuff},
        },
        error::Error,
        probe::{
            DebugProbe, DebugProbeError, IoSequenceItem, JtagAccess, JtagSequence, ProbeStatistics,
            RawSwdIo, SwdSettings, WireProtocol,
        },
    };
    use probe_rs_target::ScanChainElement;

    use super::{
        JTAG_ABORT_IR_VALUE, JTAG_ACCESS_PORT_IR_VALUE, JTAG_DEBUG_PORT_IR_VALUE,
        JTAG_DR_BIT_LENGTH, JTAG_STATUS_OK, JTAG_STATUS_WAIT,
    };

    use bitvec::prelude::*;

    #[expect(dead_code)]
    enum DapAcknowledge {
        Ok,
        Wait,
        Fault,
        NoAck,
    }

    #[derive(Debug)]
    struct ExpectedJtagTransaction {
        ir_address: u32,
        address: u32,
        value: u32,
        read: bool,
        result: u64,
    }

    #[derive(Debug)]
    struct MockJaylink {
        io_input: Option<Vec<IoSequenceItem>>,
        transfer_responses: Vec<Vec<bool>>,
        jtag_transactions: Vec<ExpectedJtagTransaction>,

        expected_transfer_count: usize,
        performed_transfer_count: usize,

        swd_settings: SwdSettings,
        probe_statistics: ProbeStatistics,

        protocol: WireProtocol,

        idle_cycles: u8,
    }

    impl MockJaylink {
        fn new() -> Self {
            Self {
                io_input: None,
                transfer_responses: vec![vec![]],
                jtag_transactions: vec![],

                expected_transfer_count: 1,
                performed_transfer_count: 0,

                swd_settings: SwdSettings::default(),
                probe_statistics: ProbeStatistics::default(),

                protocol: WireProtocol::Swd,

                idle_cycles: 0,
            }
        }

        fn add_write_response(&mut self, acknowledge: DapAcknowledge, idle_cycles: usize) {
            let last_transfer = self.transfer_responses.last_mut().unwrap();

            // The write consists of the following parts:
            //
            // - 8 request bits
            // - 1 turnaround bit
            // - 3 acknowledge bits
            // - 2 turnaround bits
            // - x idle cycles
            let write_length = 8 + 1 + 3 + 2 + 32 + idle_cycles;

            let mut response = BitVec::<usize, Lsb0>::repeat(false, write_length);

            match acknowledge {
                DapAcknowledge::Ok => {
                    // Set acknowledege to OK
                    response.set(8, true);
                }
                DapAcknowledge::Wait => {
                    // Set acknowledege to WAIT
                    response.set(9, true);
                }
                DapAcknowledge::Fault => {
                    // Set acknowledege to FAULT
                    response.set(10, true);
                }
                DapAcknowledge::NoAck => {
                    // No acknowledge means that all acknowledge bits
                    // are set to false.
                }
            }

            last_transfer.extend(response);
        }

        fn add_jtag_abort(&mut self) {
            let expected = ExpectedJtagTransaction {
                ir_address: JTAG_ABORT_IR_VALUE,
                address: 0,
                value: 0,
                read: false,
                result: 0,
            };

            self.jtag_transactions.push(expected);
            self.expected_transfer_count += 1;
        }

        fn add_jtag_response<P: Into<RegisterAddress>>(
            &mut self,
            address: P,
            read: bool,
            acknowlege: DapAcknowledge,
            output_value: u32,
            input_value: u32,
        ) {
            let port = address.into();
            let address = port.lsb().into();
            let mut response = (output_value as u64) << 3;

            let status = match acknowlege {
                DapAcknowledge::Ok => JTAG_STATUS_OK,
                DapAcknowledge::Wait => JTAG_STATUS_WAIT,
                _ => 0b111,
            };

            response |= status as u64;

            let expected = ExpectedJtagTransaction {
                ir_address: if matches!(port, RegisterAddress::DpRegister(_)) {
                    JTAG_DEBUG_PORT_IR_VALUE
                } else {
                    JTAG_ACCESS_PORT_IR_VALUE
                },
                address,
                value: input_value,
                read,
                result: response,
            };

            self.jtag_transactions.push(expected);
            self.expected_transfer_count += 1;
        }

        fn add_read_response(&mut self, acknowledge: DapAcknowledge, value: u32) {
            let last_transfer = self.transfer_responses.last_mut().unwrap();

            // The read consists of the following parts:
            //
            // - 2 idle bits
            // - 8 request bits
            // - 1 turnaround bit
            // - 3 acknowledge bits
            // - 2 turnaround bits
            let write_length = 8 + 1 + 3 + 32 + 2;

            let mut response = BitVec::<usize, Lsb0>::repeat(false, write_length);

            match acknowledge {
                DapAcknowledge::Ok => {
                    // Set acknowledege to OK
                    response.set(8, true);
                }
                DapAcknowledge::Wait => {
                    // Set acknowledege to WAIT
                    response.set(9, true);
                }
                DapAcknowledge::Fault => {
                    // Set acknowledege to FAULT
                    response.set(10, true);
                }
                DapAcknowledge::NoAck => {
                    // No acknowledge means that all acknowledge bits
                    // are set to false.
                }
            }

            // Set the read value
            response.get_mut(11..11 + 32).unwrap().store_le(value);

            // calculate the parity bit
            let parity_bit = value.count_ones() % 2 == 1;
            response.set(11 + 32, parity_bit);

            last_transfer.extend(response);
        }

        fn add_idle_cycles(&mut self, len: usize) {
            let last_transfer = self.transfer_responses.last_mut().unwrap();

            last_transfer.extend(std::iter::repeat_n(false, len))
        }

        fn add_transfer(&mut self) {
            self.transfer_responses.push(Vec::new());
            self.expected_transfer_count += 1;
        }
    }

    impl JtagAccess for MockJaylink {
        fn shift_raw_sequence(&mut self, _: JtagSequence) -> Result<BitVec, DebugProbeError> {
            todo!()
        }

        fn set_scan_chain(&mut self, _: &[ScanChainElement]) -> Result<(), DebugProbeError> {
            todo!()
        }

        fn scan_chain(&mut self) -> Result<&[ScanChainElement], DebugProbeError> {
            todo!()
        }

        fn tap_reset(&mut self) -> Result<(), DebugProbeError> {
            todo!()
        }

        fn read_register(&mut self, _address: u32, _len: u32) -> Result<BitVec, DebugProbeError> {
            todo!()
        }

        fn set_idle_cycles(&mut self, idle_cycles: u8) -> Result<(), DebugProbeError> {
            self.idle_cycles = idle_cycles;
            Ok(())
        }

        fn idle_cycles(&self) -> u8 {
            self.idle_cycles
        }

        fn write_register(
            &mut self,
            address: u32,
            data: &[u8],
            len: u32,
        ) -> Result<BitVec, DebugProbeError> {
            let jtag_value = data[..5].view_bits::<Lsb0>().load_le::<u64>();

            // Always 35 bit transfers
            assert_eq!(len, JTAG_DR_BIT_LENGTH);

            let jtag_transaction = self.jtag_transactions.remove(0);

            assert_eq!(
                jtag_transaction.ir_address,
                address,
                "Address mismatch with {} remaining transactions",
                self.jtag_transactions.len()
            );

            if jtag_transaction.ir_address != JTAG_ABORT_IR_VALUE {
                let value = (jtag_value >> 3) as u32;
                let rnw = jtag_value & 1 == 1;
                let dap_address = ((jtag_value & 0x6) << 1) as u32;

                assert_eq!(dap_address, jtag_transaction.address);
                assert_eq!(rnw, jtag_transaction.read);
                assert_eq!(value, jtag_transaction.value);
            }

            self.performed_transfer_count += 1;

            let ret = jtag_transaction.result;

            let mut ret_vec = BitVec::new();
            ret_vec.extend_from_bitslice(ret.to_le_bytes()[..5].view_bits::<Lsb0>());

            Ok(ret_vec)
        }

        fn write_dr(&mut self, _data: &[u8], _len: u32) -> Result<BitVec, DebugProbeError> {
            unimplemented!()
        }
    }

    impl RawSwdIo for MockJaylink {
        fn swd_io<S>(&mut self, swdio: S) -> Result<Vec<bool>, DebugProbeError>
        where
            S: IntoIterator<Item = IoSequenceItem>,
        {
            self.io_input = Some(swdio.into_iter().collect());

            let transfer_response = self.transfer_responses.remove(0);

            let io_bits = self.io_input.as_ref().map(|v| v.len()).unwrap();
            assert_eq!(
                transfer_response.len(),
                io_bits,
                "Length mismatch for transfer {}/{}. Transferred {} bits, expected {}",
                self.performed_transfer_count + 1,
                self.expected_transfer_count,
                io_bits,
                transfer_response.len(),
            );

            self.performed_transfer_count += 1;

            Ok(transfer_response)
        }

        fn swj_pins(
            &mut self,
            _pin_out: u32,
            _pin_select: u32,
            _pin_wait: u32,
        ) -> Result<u32, DebugProbeError> {
            Err(DebugProbeError::CommandNotSupportedByProbe {
                command_name: "swj_pins",
            })
        }

        fn swd_settings(&self) -> &SwdSettings {
            &self.swd_settings
        }

        fn probe_statistics(&mut self) -> &mut ProbeStatistics {
            &mut self.probe_statistics
        }
    }

    /// This is just a blanket impl that will crash if used (only relevant in tests,
    /// so no problem as we do not use it) to fulfill the marker requirement.
    impl DebugProbe for MockJaylink {
        fn get_name(&self) -> &str {
            todo!()
        }

        fn speed_khz(&self) -> u32 {
            todo!()
        }

        fn set_speed(&mut self, _speed_khz: u32) -> Result<u32, DebugProbeError> {
            todo!()
        }

        fn attach(&mut self) -> Result<(), DebugProbeError> {
            todo!()
        }

        fn detach(&mut self) -> Result<(), Error> {
            todo!()
        }

        fn target_reset(&mut self) -> Result<(), DebugProbeError> {
            todo!()
        }

        fn target_reset_assert(&mut self) -> Result<(), DebugProbeError> {
            todo!()
        }

        fn target_reset_deassert(&mut self) -> Result<(), DebugProbeError> {
            todo!()
        }

        fn select_protocol(&mut self, protocol: WireProtocol) -> Result<(), DebugProbeError> {
            self.protocol = protocol;

            Ok(())
        }

        fn active_protocol(&self) -> Option<WireProtocol> {
            Some(self.protocol)
        }

        fn into_probe(self: Box<Self>) -> Box<dyn DebugProbe> {
            todo!()
        }
    }

    #[test]
    fn read_register() {
        let read_value = 12;

        let mut mock = MockJaylink::new();

        mock.add_read_response(DapAcknowledge::Ok, 0);
        mock.add_read_response(DapAcknowledge::Ok, read_value);
        mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

        let result = mock.raw_read_register(ApAddress::V1(4).into()).unwrap();

        assert_eq!(result, read_value);
    }

    #[test]
    fn read_register_jtag() {
        let read_value = 12;

        let mut mock = MockJaylink::new();

        let result = mock.select_protocol(WireProtocol::Jtag);
        assert!(result.is_ok());

        // Read request
        mock.add_jtag_response(ApAddress::V1(4), true, DapAcknowledge::Ok, 0, 0);
        mock.add_jtag_response(RdBuff::ADDRESS, true, DapAcknowledge::Ok, read_value, 0);
        // Check CTRL
        mock.add_jtag_response(Ctrl::ADDRESS, true, DapAcknowledge::Ok, 0, 0);
        mock.add_jtag_response(RdBuff::ADDRESS, true, DapAcknowledge::Ok, 0, 0);

        let result = mock.raw_read_register(ApAddress::V1(4).into()).unwrap();

        assert_eq!(result, read_value);
    }

    #[test]
    fn read_register_with_wait_response() {
        let read_value = 47;
        let mut mock = MockJaylink::new();

        mock.add_read_response(DapAcknowledge::Ok, 0);
        mock.add_read_response(DapAcknowledge::Wait, 0);
        mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

        //  When a wait response is received, the sticky overrun bit has to be cleared

        mock.add_transfer();
        mock.add_write_response(
            DapAcknowledge::Ok,
            mock.swd_settings.num_idle_cycles_between_writes,
        );
        mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

        mock.add_transfer();
        //mock.add_read_response(DapAcknowledge::Ok, 0);
        mock.add_read_response(DapAcknowledge::Ok, read_value);
        mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

        let result = mock.raw_read_register(ApAddress::V1(4).into()).unwrap();

        assert_eq!(result, read_value);
    }

    #[test]
    fn read_register_with_wait_response_jtag() {
        let read_value = 47;
        let mut mock = MockJaylink::new();

        let result = mock.select_protocol(WireProtocol::Jtag);
        assert!(result.is_ok());

        // Read
        mock.add_jtag_response(ApAddress::V1(4), true, DapAcknowledge::Ok, 0, 0);
        mock.add_jtag_response(RdBuff::ADDRESS, true, DapAcknowledge::Wait, 0, 0);

        //  When a wait response is received, the sticky overrun bit has to be cleared
        mock.add_jtag_abort();

        // Retry
        mock.add_jtag_response(ApAddress::V1(4), true, DapAcknowledge::Ok, 0, 0);
        mock.add_jtag_response(RdBuff::ADDRESS, true, DapAcknowledge::Ok, read_value, 0);
        // Check CTRL
        mock.add_jtag_response(Ctrl::ADDRESS, true, DapAcknowledge::Ok, 0, 0);
        mock.add_jtag_response(RdBuff::ADDRESS, true, DapAcknowledge::Ok, 0, 0);

        let result = mock.raw_read_register(ApAddress::V1(4).into()).unwrap();

        assert_eq!(result, read_value);
    }

    #[test]
    fn write_register() {
        let mut mock = MockJaylink::new();

        let idle_cycles = mock.swd_settings.num_idle_cycles_between_writes;

        mock.add_write_response(DapAcknowledge::Ok, idle_cycles);
        mock.add_idle_cycles(mock.swd_settings.idle_cycles_before_write_verify);
        mock.add_read_response(DapAcknowledge::Ok, 0);
        mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

        mock.raw_write_register(ApAddress::V1(4).into(), 0x123)
            .expect("Failed to write register");
    }

    #[test]
    fn write_register_jtag() {
        let mut mock = MockJaylink::new();

        let result = mock.select_protocol(WireProtocol::Jtag);
        assert!(result.is_ok());

        mock.add_jtag_response(ApAddress::V1(4), false, DapAcknowledge::Ok, 0x0, 0x123);
        mock.add_jtag_response(RdBuff::ADDRESS, true, DapAcknowledge::Ok, 0x123, 0x0);
        // Check CTRL
        mock.add_jtag_response(Ctrl::ADDRESS, true, DapAcknowledge::Ok, 0, 0);
        mock.add_jtag_response(RdBuff::ADDRESS, true, DapAcknowledge::Ok, 0, 0);

        mock.raw_write_register(ApAddress::V1(4).into(), 0x123)
            .expect("Failed to write register");
    }

    #[test]
    fn write_register_with_wait_response() {
        let mut mock = MockJaylink::new();
        let idle_cycles = mock.swd_settings.num_idle_cycles_between_writes;

        mock.add_write_response(DapAcknowledge::Ok, idle_cycles);
        mock.add_idle_cycles(mock.swd_settings.idle_cycles_before_write_verify);
        mock.add_read_response(DapAcknowledge::Wait, 0);
        mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

        // Expect a Write to the ABORT register.
        mock.add_transfer();
        mock.add_write_response(DapAcknowledge::Ok, idle_cycles);
        mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

        // Second try to write register, with increased idle cycles.
        mock.add_transfer();
        //mock.add_write_response(DapAcknowledge::Ok, idle_cycles * 2);
        //mock.add_idle_cycles(mock.swd_settings.idle_cycles_before_write_verify);
        mock.add_read_response(DapAcknowledge::Ok, 0);
        mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

        mock.raw_write_register(ApAddress::V1(4).into(), 0x123)
            .expect("Failed to write register");
    }

    #[test]
    fn write_register_with_wait_response_jtag() {
        let mut mock = MockJaylink::new();

        let result = mock.select_protocol(WireProtocol::Jtag);
        assert!(result.is_ok());

        mock.add_jtag_response(ApAddress::V1(4), false, DapAcknowledge::Ok, 0x0, 0x123);
        mock.add_jtag_response(RdBuff::ADDRESS, true, DapAcknowledge::Wait, 0x0, 0x0);

        // Expect a Write to the ABORT register.
        mock.add_jtag_abort();

        // Second try to write register.
        mock.add_jtag_response(ApAddress::V1(4), false, DapAcknowledge::Ok, 0x0, 0x123);
        mock.add_jtag_response(RdBuff::ADDRESS, true, DapAcknowledge::Ok, 0x123, 0x0);
        // Check CTRL
        mock.add_jtag_response(Ctrl::ADDRESS, true, DapAcknowledge::Ok, 0, 0);
        mock.add_jtag_response(RdBuff::ADDRESS, true, DapAcknowledge::Ok, 0, 0);

        mock.raw_write_register(ApAddress::V1(4).into(), 0x123)
            .expect("Failed to write register");
    }

    /// Test the correct handling of several transfers, with
    /// the appropriate extra reads added as necessary.
    mod transfer_handling {
        use super::{
            super::{DapTransfer, TransferStatus, perform_transfers},
            DapAcknowledge, MockJaylink,
        };
        use crate::architecture::arm::{
            ApAddress,
            dp::{Abort, Ctrl, DPIDR, DpRegister, DpRegisterAddress},
        };

        #[test]
        fn single_dp_register_read() {
            let register_value = 32354;

            let mut transfers = vec![DapTransfer::read(DPIDR::ADDRESS)];

            let mut mock = MockJaylink::new();

            mock.add_read_response(DapAcknowledge::Ok, register_value);
            mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

            perform_transfers(&mut mock, &mut transfers).expect("Failed to perform transfer");

            let transfer_result = &transfers[0];

            assert_eq!(transfer_result.status, TransferStatus::Ok);
            assert_eq!(transfer_result.value, register_value);
        }

        #[test]
        fn single_ap_register_read() {
            let register_value = 0x11_22_33_44u32;

            let mut transfers = vec![DapTransfer::read(ApAddress::V1(0))];

            let mut mock = MockJaylink::new();

            mock.add_read_response(DapAcknowledge::Ok, 0);
            mock.add_read_response(DapAcknowledge::Ok, register_value);
            mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

            perform_transfers(&mut mock, &mut transfers).expect("Failed to perform transfer");

            let transfer_result = &transfers[0];

            assert_eq!(transfer_result.status, TransferStatus::Ok);
            assert_eq!(transfer_result.value, register_value);
        }

        #[test]
        fn ap_then_dp_register_read() {
            // When reading from the AP first, and then from the DP,
            // we need to insert an additional read from the RDBUFF register to
            // get the result for the AP read.

            let ap_read_value = 0x123223;
            let dp_read_value = 0xFFAABB;

            let mut transfers = vec![
                DapTransfer::read(ApAddress::V1(4)),
                DapTransfer::read(DpRegisterAddress {
                    address: 3,
                    bank: None,
                }),
            ];

            let mut mock = MockJaylink::new();

            mock.add_read_response(DapAcknowledge::Ok, 0);
            mock.add_read_response(DapAcknowledge::Ok, ap_read_value);
            mock.add_read_response(DapAcknowledge::Ok, dp_read_value);
            mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

            perform_transfers(&mut mock, &mut transfers).expect("Failed to perform transfer");

            assert_eq!(transfers[0].status, TransferStatus::Ok);
            assert_eq!(transfers[0].value, ap_read_value);

            assert_eq!(transfers[1].status, TransferStatus::Ok);
            assert_eq!(transfers[1].value, dp_read_value);
        }

        #[test]
        fn dp_then_ap_register_read() {
            // When reading from the DP first, and then from the AP,
            // we need to insert an additional read from the RDBUFF register at the end
            // to get the result for the AP read.

            let ap_read_value = 0x123223;
            let dp_read_value = 0xFFAABB;

            let mut transfers = vec![
                DapTransfer::read(DpRegisterAddress {
                    address: 3,
                    bank: None,
                }),
                DapTransfer::read(ApAddress::V1(4)),
            ];

            let mut mock = MockJaylink::new();

            mock.add_read_response(DapAcknowledge::Ok, dp_read_value);
            mock.add_read_response(DapAcknowledge::Ok, 0);
            mock.add_read_response(DapAcknowledge::Ok, ap_read_value);
            mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

            perform_transfers(&mut mock, &mut transfers).expect("Failed to perform transfer");

            assert_eq!(transfers[0].status, TransferStatus::Ok);
            assert_eq!(transfers[0].value, dp_read_value);

            assert_eq!(transfers[1].status, TransferStatus::Ok);
            assert_eq!(transfers[1].value, ap_read_value);
        }

        #[test]
        fn multiple_ap_read() {
            // When reading from the AP twice, only a single additional read from the
            // RDBUFF register is necessary.

            let ap_read_values = [1, 2];

            let mut transfers = vec![
                DapTransfer::read(ApAddress::V1(4)),
                DapTransfer::read(ApAddress::V1(4)),
            ];

            let mut mock = MockJaylink::new();

            mock.add_read_response(DapAcknowledge::Ok, 0);
            mock.add_read_response(DapAcknowledge::Ok, ap_read_values[0]);
            mock.add_read_response(DapAcknowledge::Ok, ap_read_values[1]);
            mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

            perform_transfers(&mut mock, &mut transfers).expect("Failed to perform transfer");

            assert_eq!(transfers[0].status, TransferStatus::Ok);
            assert_eq!(transfers[0].value, ap_read_values[0]);

            assert_eq!(transfers[1].status, TransferStatus::Ok);
            assert_eq!(transfers[1].value, ap_read_values[1]);
        }

        #[test]
        fn multiple_dp_read() {
            // When reading from the DP twice, no additional reads have to be inserted.

            let dp_read_values = [1, 2];

            let mut transfers = vec![
                DapTransfer::read(Ctrl::ADDRESS),
                DapTransfer::read(Ctrl::ADDRESS),
            ];

            let mut mock = MockJaylink::new();

            mock.add_read_response(DapAcknowledge::Ok, dp_read_values[0]);
            mock.add_read_response(DapAcknowledge::Ok, dp_read_values[1]);
            mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

            perform_transfers(&mut mock, &mut transfers).expect("Failed to perform transfer");

            assert_eq!(transfers[0].status, TransferStatus::Ok);
            assert_eq!(transfers[0].value, dp_read_values[0]);

            assert_eq!(transfers[1].status, TransferStatus::Ok);
            assert_eq!(transfers[1].value, dp_read_values[1]);
        }

        #[test]
        fn single_dp_register_write() {
            let mut transfers = vec![DapTransfer::write(Abort::ADDRESS, 0x1234_5678)];

            let mut mock = MockJaylink::new();

            mock.add_write_response(
                DapAcknowledge::Ok,
                mock.swd_settings.num_idle_cycles_between_writes,
            );

            // To verify that the write was successful, an additional read is performed.
            mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

            perform_transfers(&mut mock, &mut transfers).expect("Failed to perform transfer");

            let transfer_result = &transfers[0];

            assert_eq!(transfer_result.status, TransferStatus::Ok);
        }

        #[test]
        fn single_ap_register_write() {
            let mut transfers = vec![DapTransfer::write(ApAddress::V1(0), 0x1234_5678)];

            let mut mock = MockJaylink::new();

            mock.add_write_response(
                DapAcknowledge::Ok,
                mock.swd_settings.num_idle_cycles_between_writes,
            );

            // To verify that the write was successful, an additional read is performed.
            mock.add_idle_cycles(mock.swd_settings.idle_cycles_before_write_verify);
            mock.add_read_response(DapAcknowledge::Ok, 0);
            mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

            perform_transfers(&mut mock, &mut transfers).expect("Failed to perform transfer");

            let transfer_result = &transfers[0];

            assert_eq!(transfer_result.status, TransferStatus::Ok);
        }

        #[test]
        fn multiple_ap_register_write() {
            let mut transfers = vec![
                DapTransfer::write(ApAddress::V1(0), 0x1234_5678),
                DapTransfer::write(ApAddress::V1(0), 0xABABABAB),
            ];

            let mut mock = MockJaylink::new();

            mock.add_write_response(
                DapAcknowledge::Ok,
                mock.swd_settings.num_idle_cycles_between_writes,
            );
            mock.add_write_response(
                DapAcknowledge::Ok,
                mock.swd_settings.num_idle_cycles_between_writes,
            );

            mock.add_idle_cycles(mock.swd_settings.idle_cycles_before_write_verify);
            mock.add_read_response(DapAcknowledge::Ok, 0);
            mock.add_idle_cycles(mock.swd_settings.idle_cycles_after_transfer);

            perform_transfers(&mut mock, &mut transfers).expect("Failed to perform transfer");

            assert_eq!(transfers[0].status, TransferStatus::Ok);
            assert_eq!(transfers[1].status, TransferStatus::Ok);
        }
    }
}
