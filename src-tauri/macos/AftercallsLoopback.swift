// AftercallsLoopback.swift
//
// ScreenCaptureKit-based system-audio loopback for the aftercalls
// agent. Captures the macOS system audio mix to a 16-bit PCM WAV file
// at the path provided by the Rust caller (paired with `system.wav`
// in the active session directory).
//
// macOS 13+ floor — SCK's audio capture support landed in Ventura.
// Earlier macOS hits the Rust-side bail-out and the user records
// mic-only (same fallback shape as the existing platforms).
//
// The Rust bridge (`src/macos_loopback.rs`) calls `start()` and
// `stop()` synchronously — both wrap the SCK async API behind a
// dispatch semaphore so the FFI surface stays sync. Errors during
// start/stop are logged via `os_log`/stderr and surfaced through the
// `lastError` accessor; the bridge inspects it and returns a Rust
// `Result` from the wrapper so a failed start doesn't kill the
// session, it just falls back to mic-only (matching Linux/Windows).

import AVFoundation
import CoreGraphics
import CoreMedia
import Foundation
import ScreenCaptureKit

@available(macOS 13.0, *)
public class AftercallsLoopback: NSObject, SCStreamOutput, SCStreamDelegate {
    // Output WAV configuration. 48 kHz / 2-channel / 16-bit PCM —
    // matches the SCStreamConfiguration we set below and keeps the
    // file interchangeable with the Linux parec output (which is also
    // s16le when fed through ffmpeg downstream).
    private static let sampleRate: Double = 48_000
    private static let channelCount: UInt32 = 2
    private static let bitsPerSample: UInt32 = 16

    private let outputPath: String
    private var fileHandle: FileHandle?
    private var stream: SCStream?
    private var bytesWritten: UInt32 = 0
    private var lastErrorMessage: String?
    // Sync-FFI result holders. Stored as instance properties (not local
    // `var`s captured by the Task closure) because Swift's strict
    // concurrency mode rejects mutation of captured local vars from
    // concurrently-executing code. The DispatchSemaphore in start()/
    // stop() handles ordering; ordering between the Task write and the
    // post-`semaphore.wait()` read is happens-before through the
    // semaphore signal.
    private var lastStartResult: Bool = false
    private var lastStopResult: Bool = false
    private let writeQueue = DispatchQueue(label: "com.aftercalls.loopback.write")

    // Parameter label matches the Rust bridge's `output_path: String`
    // — swift-bridge generates a wrapper that passes a `RustString` to
    // an init expecting `output_path:`. Don't rename to camelCase or
    // the auto-generated agent.swift wrapper won't compile.
    public init(output_path: RustString) {
        self.outputPath = output_path.toString()
        super.init()
    }

    /// Synchronous start — blocks the caller's thread on the SCK
    /// async setup via a semaphore. Returns true on success; on
    /// failure, `lastError()` exposes the message for the Rust side.
    public func start() -> Bool {
        let semaphore = DispatchSemaphore(value: 0)
        lastStartResult = false

        Task {
            do {
                try await self.startAsync()
                self.lastStartResult = true
            } catch {
                self.lastErrorMessage = "start: \(error.localizedDescription)"
                FileHandle.standardError.write(
                    Data("aftercalls/macos: SCK start failed: \(error)\n".utf8))
            }
            semaphore.signal()
        }

        semaphore.wait()
        return lastStartResult
    }

    /// Synchronous stop — same semaphore pattern as `start`. Idempotent:
    /// calling stop twice is a no-op on the second call.
    public func stop() -> Bool {
        guard stream != nil else { return true }
        let semaphore = DispatchSemaphore(value: 0)
        lastStopResult = false

        Task {
            do {
                try await self.stopAsync()
                self.lastStopResult = true
            } catch {
                self.lastErrorMessage = "stop: \(error.localizedDescription)"
                FileHandle.standardError.write(
                    Data("aftercalls/macos: SCK stop failed: \(error)\n".utf8))
            }
            semaphore.signal()
        }

        semaphore.wait()
        return lastStopResult
    }

    /// Returns the last error message (or empty string if none).
    /// Bridged into Rust as a `String` for log surfacing.
    public func lastError() -> String {
        return lastErrorMessage ?? ""
    }

    // MARK: - Async implementations

    private func startAsync() async throws {
        // Open the WAV file with a placeholder header. We patch the
        // RIFF + data chunk sizes on stop() once we know how many
        // PCM bytes we wrote.
        FileManager.default.createFile(atPath: outputPath, contents: nil, attributes: nil)
        guard let handle = FileHandle(forWritingAtPath: outputPath) else {
            throw NSError(
                domain: "AftercallsLoopback", code: 1,
                userInfo: [NSLocalizedDescriptionKey: "failed to open \(outputPath)"])
        }
        self.fileHandle = handle
        try handle.write(contentsOf: makeWavHeader(dataSize: 0))
        bytesWritten = 0

        // SCShareableContent lists everything we *could* capture.
        // For system-audio loopback we pick any display — SCK ties
        // the audio stream to a content filter, but the audio mix
        // itself is system-wide regardless of which display owns
        // the filter.
        let content = try await SCShareableContent.current
        guard let display = content.displays.first else {
            throw NSError(
                domain: "AftercallsLoopback", code: 2,
                userInfo: [NSLocalizedDescriptionKey: "no displays available"])
        }

        let filter = SCContentFilter(display: display, excludingWindows: [])

        let config = SCStreamConfiguration()
        config.capturesAudio = true
        config.excludesCurrentProcessAudio = true
        config.sampleRate = Int(Self.sampleRate)
        config.channelCount = Int(Self.channelCount)
        // Keep video minimal — we only want audio, but SCK requires
        // a non-zero video config. 2x2 / 1 fps is the lightest path.
        config.width = 2
        config.height = 2
        config.minimumFrameInterval = CMTime(value: 1, timescale: 1)
        config.queueDepth = 5

        let stream = SCStream(filter: filter, configuration: config, delegate: self)
        try stream.addStreamOutput(
            self, type: .audio, sampleHandlerQueue: writeQueue)
        try await stream.startCapture()
        self.stream = stream
    }

    private func stopAsync() async throws {
        // Stop the capture first, capturing any error to re-throw after
        // the file is sealed. Even if stopCapture fails, we MUST patch
        // the WAV header so the bytes already on disk are readable —
        // otherwise dataSize stays 0 and downstream players (QuickTime,
        // ffmpeg) reject the file as malformed.
        var captureError: Error?
        if let stream = self.stream {
            do {
                try await stream.stopCapture()
            } catch {
                captureError = error
            }
            self.stream = nil
        }

        // Drain the write-queue before sealing the file so any
        // in-flight sampleBuffer callbacks have finished writing.
        // `sync` blocks until the queue is empty.
        writeQueue.sync {}

        // Patch the WAV header in place: bytes 0..44 = riffSize +
        // dataSize. Standard 44-byte PCM RIFF/WAVE/fmt /data preamble.
        if let handle = fileHandle {
            try handle.synchronize()
            try handle.seek(toOffset: 0)
            let header = makeWavHeader(dataSize: bytesWritten)
            try handle.write(contentsOf: header)
            try handle.close()
            fileHandle = nil
        }

        if let captureError = captureError {
            throw captureError
        }
    }

    // MARK: - SCStreamOutput

    public func stream(
        _ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
        of outputType: SCStreamOutputType
    ) {
        guard outputType == .audio,
              CMSampleBufferDataIsReady(sampleBuffer),
              let formatDesc = CMSampleBufferGetFormatDescription(sampleBuffer),
              let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(formatDesc)?.pointee
        else { return }

        var blockBufferOut: CMBlockBuffer?
        var audioBufferList = AudioBufferList()
        let status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sampleBuffer,
            bufferListSizeNeededOut: nil,
            bufferListOut: &audioBufferList,
            bufferListSize: MemoryLayout<AudioBufferList>.size,
            blockBufferAllocator: nil,
            blockBufferMemoryAllocator: nil,
            flags: kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            blockBufferOut: &blockBufferOut)
        guard status == noErr else { return }

        // SCK gives us non-interleaved Float32 by default. Convert to
        // interleaved 16-bit signed PCM to match the WAV header.
        //
        // Use UnsafeMutableAudioBufferListPointer to walk the trailing
        // `mNumberBuffers` array safely. The naive
        // `withUnsafePointer(to: &audioBufferList.mBuffers)` idiom is
        // unsafe when mNumberBuffers > 1 — Swift's struct layout for
        // AudioBufferList doesn't lay out the trailing buffers
        // contiguously, so reading `buffers[1]` etc. would dereference
        // unrelated memory. The pointer wrapper handles the C-style
        // trailing-array layout correctly.
        let bufferList = UnsafeMutableAudioBufferListPointer(
            UnsafeMutablePointer(&audioBufferList))
        let isFloat = (asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0
        let isInterleaved = (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) == 0

        let frameCount = Int(CMSampleBufferGetNumSamples(sampleBuffer))
        let outChannels = Int(Self.channelCount)
        var interleaved = [Int16](repeating: 0, count: frameCount * outChannels)

        if isFloat && !isInterleaved && bufferList.count >= 1 {
            // Non-interleaved float buffers, one per channel.
            for ch in 0..<min(outChannels, bufferList.count) {
                guard let ptr = bufferList[ch].mData?.assumingMemoryBound(to: Float32.self)
                else { continue }
                for f in 0..<frameCount {
                    let sample = ptr[f]
                    let clamped = max(-1.0, min(1.0, sample))
                    interleaved[f * outChannels + ch] = Int16(clamped * 32767.0)
                }
            }
        } else if isFloat && isInterleaved, let buf = bufferList.first,
                  let ptr = buf.mData?.assumingMemoryBound(to: Float32.self) {
            // Already interleaved float.
            let totalSamples = frameCount * outChannels
            for i in 0..<totalSamples {
                let clamped = max(-1.0, min(1.0, ptr[i]))
                interleaved[i] = Int16(clamped * 32767.0)
            }
        } else {
            // Unsupported format — drop the buffer rather than write
            // garbage. Logged so a real-hardware run surfaces the
            // mismatch quickly.
            FileHandle.standardError.write(
                Data("aftercalls/macos: unexpected SCK audio format flags=\(asbd.mFormatFlags)\n"
                    .utf8))
            return
        }

        let data = interleaved.withUnsafeBufferPointer { Data(buffer: $0) }
        do {
            try fileHandle?.write(contentsOf: data)
            bytesWritten = bytesWritten &+ UInt32(data.count)
        } catch {
            FileHandle.standardError.write(
                Data("aftercalls/macos: write failed: \(error)\n".utf8))
        }
    }

    // MARK: - SCStreamDelegate

    public func stream(_ stream: SCStream, didStopWithError error: Error) {
        lastErrorMessage = "stream stopped: \(error.localizedDescription)"
        FileHandle.standardError.write(
            Data("aftercalls/macos: stream stopped with error: \(error)\n".utf8))
    }

    // MARK: - WAV header

    /// Build a 44-byte canonical RIFF/WAVE header for 16-bit PCM. The
    /// `dataSize` argument is patched on stop(); during start() we
    /// write it as 0 and overwrite on close.
    private func makeWavHeader(dataSize: UInt32) -> Data {
        let channels = UInt16(Self.channelCount)
        let sampleRate = UInt32(Self.sampleRate)
        let bitsPerSample = UInt16(Self.bitsPerSample)
        let byteRate = sampleRate * UInt32(channels) * UInt32(bitsPerSample / 8)
        let blockAlign = channels * (bitsPerSample / 8)
        let riffSize = dataSize == 0 ? 0 : dataSize + 36

        var header = Data()
        header.append(contentsOf: "RIFF".utf8)
        header.append(uint32LE(riffSize))
        header.append(contentsOf: "WAVE".utf8)
        header.append(contentsOf: "fmt ".utf8)
        header.append(uint32LE(16))           // PCM fmt chunk size
        header.append(uint16LE(1))            // format = PCM
        header.append(uint16LE(channels))
        header.append(uint32LE(sampleRate))
        header.append(uint32LE(byteRate))
        header.append(uint16LE(blockAlign))
        header.append(uint16LE(bitsPerSample))
        header.append(contentsOf: "data".utf8)
        header.append(uint32LE(dataSize))
        return header
    }

    private func uint32LE(_ v: UInt32) -> Data {
        var le = v.littleEndian
        return Data(bytes: &le, count: 4)
    }

    private func uint16LE(_ v: UInt16) -> Data {
        var le = v.littleEndian
        return Data(bytes: &le, count: 2)
    }
}

// MARK: - TCC permission pre-flight (#623, free functions)
//
// Bridged into Rust as free functions via the `ffi` module in
// `src/macos_loopback.rs`. Kept outside the `@available(macOS 13.0, *)`
// class because the permission APIs (AVFoundation since 10.14,
// CoreGraphics screen-capture preflight since 10.15) have a lower
// floor than ScreenCaptureKit and the agent should report grant state
// even on a macOS older than the SCK loopback floor.

/// Current microphone authorization, as the raw `AVAuthorizationStatus`
/// value: .notDetermined=0, .restricted=1, .denied=2, .authorized=3.
/// The Rust side maps these to `PermStatus`.
public func micAuthStatus() -> Int32 {
    return Int32(AVCaptureDevice.authorizationStatus(for: .audio).rawValue)
}

/// Whether the app already has screen-recording (system-audio loopback)
/// access. `CGPreflightScreenCaptureAccess` is non-prompting — it just
/// reads the current TCC grant.
public func screenCaptureAuthStatus() -> Bool {
    return CGPreflightScreenCaptureAccess()
}

/// Prompt for screen-recording access (or no-op if a decision already
/// exists) and return the resulting grant. The macOS prompt directs the
/// user to System Settings; the grant only takes effect on the next
/// capture attempt, so callers should re-check after the user returns.
public func requestScreenCaptureAccess() -> Bool {
    return CGRequestScreenCaptureAccess()
}

/// Fire the AVFoundation microphone-permission prompt. Async on the OS
/// side; we don't surface the completion bool here — the Rust caller
/// re-reads `micAuthStatus()` for the authoritative value once the user
/// has answered.
public func requestMicAccess() {
    AVCaptureDevice.requestAccess(for: .audio) { _ in }
}
