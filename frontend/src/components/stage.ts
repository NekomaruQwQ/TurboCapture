/// Logical dimensions used to author the frontend composition.
export const STAGE_WIDTH = 1280;
export const STAGE_HEIGHT = 720;

/// Placement of the logical broadcast stage within a host-provided surface.
export type StageLayout = {
    /// Uniform scale that preserves the authored aspect ratio.
    readonly scale: number;
    /// Horizontal offset in host CSS pixels after scaling.
    readonly left: number;
    /// Vertical offset in host CSS pixels after scaling.
    readonly top: number;
};

const HIDDEN_LAYOUT: StageLayout = {
    scale: 0,
    left: 0,
    top: 0,
};

/**
 * Fits the fixed frontend composition inside a host surface without cropping
 * or stretching it.
 *
 * Invalid or non-positive dimensions return a hidden layout. ResizeObserver
 * can report a transient zero-sized box while a host is being created, and
 * collapsing the stage avoids propagating NaN or infinity into CSS.
 */
export function fitStage(
    hostWidth: number,
    hostHeight: number,
): StageLayout {
    if (
        !Number.isFinite(hostWidth)
        || !Number.isFinite(hostHeight)
        || hostWidth <= 0
        || hostHeight <= 0
    ) {
        return HIDDEN_LAYOUT;
    }

    const scale = Math.min(
        hostWidth / STAGE_WIDTH,
        hostHeight / STAGE_HEIGHT,
    );

    return {
        scale,
        left: (hostWidth - STAGE_WIDTH * scale) / 2,
        top: (hostHeight - STAGE_HEIGHT * scale) / 2,
    };
}
