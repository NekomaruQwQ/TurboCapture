/// <reference types="bun" />

import { describe, expect, test } from "bun:test";
import { fitStage } from "./stage";

describe("fitStage", () => {
    test("preserves the authored size on an exact host", () => {
        expect(fitStage(1280, 720)).toEqual({
            scale: 1,
            left: 0,
            top: 0,
        });
    });

    test("uniformly upscales a matching host", () => {
        expect(fitStage(1920, 1080)).toEqual({
            scale: 1.5,
            left: 0,
            top: 0,
        });
    });

    test("centers vertical letterboxing when width is limiting", () => {
        expect(fitStage(1024, 768)).toEqual({
            scale: 0.8,
            left: 0,
            top: 96,
        });
    });

    test("centers vertical letterboxing around an upscaled stage", () => {
        expect(fitStage(1920, 1200)).toEqual({
            scale: 1.5,
            left: 0,
            top: 60,
        });
    });

    test("centers horizontal letterboxing when height is limiting", () => {
        expect(fitStage(1600, 720)).toEqual({
            scale: 1,
            left: 160,
            top: 0,
        });
    });

    const invalidSizes: [number, number][] = [
        [0, 720],
        [1280, 0],
        [Number.NaN, 720],
        [1280, Number.POSITIVE_INFINITY],
    ];

    test.each(invalidSizes)("hides the stage for an invalid %p x %p host", (width, height) => {
        expect(fitStage(width, height)).toEqual({
            scale: 0,
            left: 0,
            top: 0,
        });
    });
});
