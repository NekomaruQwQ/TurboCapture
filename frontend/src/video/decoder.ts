import type { AccessUnit, CodecConfiguration } from "../protocol";

/** Maximum number of chunks allowed to wait inside WebCodecs. */
export const MAX_DECODE_QUEUE_SIZE = 4;

/** Signals that decoding cannot keep up and a clean reconnect is safer. */
export class DecoderBacklogError extends Error {
  constructor(queueSize: number) {
    super(`WebCodecs decode queue reached ${queueSize} chunks.`);
    this.name = "DecoderBacklogError";
  }
}

/** Thin generation-aware H.264/AVCC wrapper around the browser decoder. */
export class H264Decoder {
  private readonly decoder: VideoDecoder;
  private generation: number | null = null;
  private waitingForKeyframe = true;

  /**
   * Creates one decoder for a single WebSocket connection.
   * Decoder errors are fatal because WebCodecs closes itself after reporting one.
   */
  constructor(onFrame: (frame: VideoFrame) => void, onError: (error: DOMException) => void) {
    this.decoder = new VideoDecoder({ output: onFrame, error: onError });
  }

  /** Applies a newer codec generation and drops queued work from the old one. */
  configure(configuration: CodecConfiguration): void {
    if (this.generation !== null && configuration.generation <= this.generation) {
      return;
    }
    if (this.decoder.state === "configured") {
      this.decoder.reset();
    }
    this.decoder.configure(configuration.decoderConfig);
    this.generation = configuration.generation;
    this.waitingForKeyframe = true;
  }

  /**
   * Decodes a matching access unit, ignoring stale generations and pre-key deltas.
   * Throws when the bounded browser queue indicates that rendering has fallen behind.
   */
  decode(accessUnit: AccessUnit): void {
    if (this.generation !== accessUnit.generation) {
      return;
    }
    if (this.waitingForKeyframe && !accessUnit.keyframe) {
      return;
    }
    if (this.decoder.decodeQueueSize >= MAX_DECODE_QUEUE_SIZE) {
      throw new DecoderBacklogError(this.decoder.decodeQueueSize);
    }

    this.decoder.decode(new EncodedVideoChunk({
      type: accessUnit.keyframe ? "key" : "delta",
      timestamp: accessUnit.timestampUs,
      data: accessUnit.data,
    }));
    if (accessUnit.keyframe) {
      this.waitingForKeyframe = false;
    }
  }

  /** Releases codec resources and discards any queued frames. */
  close(): void {
    if (this.decoder.state !== "closed") {
      this.decoder.close();
    }
  }
}
