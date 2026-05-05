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
    private let writeQueue = DispatchQueue(label: "com.aftercalls.loopback.write")

    public init(outputPath: String) {
        self.outputPath = outputPath
        super.init()
    }

    /// Synchronous start — blocks the caller's thread on the SCK
    /// async setup via a semaphore. Returns true on success; on
    /// failure, `lastError()` exposes the message for the Rust side.
    public func start() -> Bool {
        let semaphore = DispatchSemaphore(value: 0)
        var ok = false

        Task {
            do {
                try await self.startAsync()
                ok = true
            } catch {
                self.lastErrorMessage = "start: \(error.localizedDescription)"
                FileHandle.standardError.write(
                    Data("aftercalls/macos: SCK start failed: \(error)\n".utf8))
            }
            semaphore.signal()
        }

        semaphore.wait()
        return ok
    }

    /// Synchronous stop — same semaphore pattern as `start`. Idempotent:
    /// calling stop twice is a no-op on the second call.
    public func stop() -> Bool {
        guard stream != nil else { return true }
        let semaphore = DispatchSemaphore(value: 0)
        var ok = false

        Task {
            do {
                try await self.stopAsync()
                ok = true
            } catch {
                self.lastErrorMessage = "stop: \(error.localizedDescription)"
                FileHandle.standardError.write(
                    Data("aftercalls/macos: SCK stop failed: \(error)\n".utf8))
            }
            semaphore.signal()
        }

        semaphore.wait()
        return ok
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
        guard let stream = self.stream else { return }
        try await stream.stopCapture()
        self.stream = nil

        // Patch the WAV header in place: bytes 4..8 = riffSize,
        // bytes 40..44 = dataSize. Our header layout is the standard
        // 44-byte PCM RIFF/WAVE/fmt /data preamble.
        if let handle = fileHandle {
            try handle.synchronize()
            try handle.seek(toOffset: 0)
            let header = makeWavHeader(dataSize: bytesWritten)
            try handle.write(contentsOf: header)
            try handle.close()
            fileHandle = nil
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
        let buffers = UnsafeBufferPointer<AudioBuffer>(
            start: withUnsafePointer(to: &audioBufferList.mBuffers) { $0 },
            count: Int(audioBufferList.mNumberBuffers))
        let isFloat = (asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0
        let isInterleaved = (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) == 0

        let frameCount = Int(CMSampleBufferGetNumSamples(sampleBuffer))
        let outChannels = Int(Self.channelCount)
        var interleaved = [Int16](repeating: 0, count: frameCount * outChannels)

        if isFloat && !isInterleaved && buffers.count >= 1 {
            // Non-interleaved float buffers, one per channel.
            for ch in 0..<min(outChannels, buffers.count) {
                guard let ptr = buffers[ch].mData?.assumingMemoryBound(to: Float32.self)
                else { continue }
                for f in 0..<frameCount {
                    let sample = ptr[f]
                    let clamped = max(-1.0, min(1.0, sample))
                    interleaved[f * outChannels + ch] = Int16(clamped * 32767.0)
                }
            }
        } else if isFloat && isInterleaved, let buf = buffers.first,
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
