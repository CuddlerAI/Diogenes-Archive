// Pure C# (no UnityEngine dependency) so the table can be unit-tested outside Unity.
// Spec source: docs/69 delivery format - keyCode values are Windows Virtual-Key codes
// (ASCII-aligned for letters/digits), e.g. W=87. See the demand-side sample
// (engine-data/fixtures/mc_cam1_action_camera.sample.json: "keyCode": [87]).

using System.Collections.Generic;

namespace Diogenes.EngineCapture
{
    /// <summary>
    /// Windows Virtual-Key (VK) codes used in the "keyCode" field of action_camera json.
    /// Letters A-Z map to ASCII 65-90, digits 0-9 to ASCII 48-57 (per the demand-side
    /// ASCII table). Only keys plausibly held during gameplay capture are listed;
    /// extend here (single source of truth) rather than inline in recorders.
    /// </summary>
    public static class VirtualKeys
    {
        // Mouse buttons (docs/69: whether mouse buttons belong in keyCode is a
        // provenance-level note; constants provided, recorder emission is configurable).
        public const int LeftMouseButton = 0x01;   // VK_LBUTTON
        public const int RightMouseButton = 0x02;  // VK_RBUTTON
        public const int MiddleMouseButton = 0x04; // VK_MBUTTON

        public const int Backspace = 0x08; // VK_BACK
        public const int Tab = 0x09;       // VK_TAB
        public const int Enter = 0x0D;     // VK_RETURN
        public const int Shift = 0x10;     // VK_SHIFT
        public const int Control = 0x11;   // VK_CONTROL
        public const int Alt = 0x12;       // VK_MENU
        public const int Escape = 0x1B;    // VK_ESCAPE
        public const int Space = 0x20;     // VK_SPACE

        public const int LeftArrow = 0x25;  // VK_LEFT
        public const int UpArrow = 0x26;    // VK_UP
        public const int RightArrow = 0x27; // VK_RIGHT
        public const int DownArrow = 0x28;  // VK_DOWN

        /// <summary>Digit '0'..'9' -> VK 48..57 (ASCII).</summary>
        public static int Digit(int digit) => 0x30 + digit;

        /// <summary>Letter 'A'..'Z' -> VK 65..90 (ASCII). Accepts upper- or lower-case.</summary>
        public static int Letter(char letter)
        {
            char c = char.ToUpperInvariant(letter);
            return c; // 'A' == 65 .. 'Z' == 90
        }

        // Convenience constants for the movement cluster (the hot path in captures).
        public const int W = 87;
        public const int A = 65;
        public const int S = 83;
        public const int D = 68;
        public const int Q = 81;
        public const int E = 69;
        public const int R = 82;
        public const int F = 70;
        public const int C = 67;
        public const int X = 88;
        public const int Z = 90;

        /// <summary>Name -> VK map for table-driven tests and tooling.</summary>
        public static readonly IReadOnlyDictionary<string, int> ByName =
            new Dictionary<string, int>
            {
                { "LMB", LeftMouseButton },
                { "RMB", RightMouseButton },
                { "MMB", MiddleMouseButton },
                { "Backspace", Backspace },
                { "Tab", Tab },
                { "Enter", Enter },
                { "Shift", Shift },
                { "Control", Control },
                { "Alt", Alt },
                { "Escape", Escape },
                { "Space", Space },
                { "Left", LeftArrow },
                { "Up", UpArrow },
                { "Right", RightArrow },
                { "Down", DownArrow },
                { "W", W }, { "A", A }, { "S", S }, { "D", D },
                { "Q", Q }, { "E", E }, { "R", R }, { "F", F },
                { "C", C }, { "X", X }, { "Z", Z }
            };
    }
}
