// Pure C# (no UnityEngine dependency) so it can be unit-tested outside Unity and
// reused by the EXR writer. IEEE 754 binary16 <-> binary32.
//
// Unity ships Mathf.FloatToHalf, but this file deliberately does NOT use it:
// the depth deliverable (docs/68 §6.4) is a byte-level contract, so the rounding
// mode has to be pinned down and tested here rather than inherited from an engine
// helper whose behaviour is not part of any contract we control.

using System;
using System.Runtime.InteropServices;

namespace Diogenes.EngineCapture
{
    /// <summary>
    /// IEEE 754 binary16 conversion with round-to-nearest-even, the rounding mode
    /// OpenEXR readers assume. NaN maps to 0x7E00; overflow maps to +/-Inf;
    /// binary32 subnormals flush to signed zero (they are far below any depth we emit).
    /// </summary>
    public static class HalfFloat
    {
        [StructLayout(LayoutKind.Explicit)]
        struct FloatBits
        {
            [FieldOffset(0)] public float F;
            [FieldOffset(0)] public uint U;
        }

        /// <summary>binary32 -> binary16 bit pattern.</summary>
        public static ushort FromFloat(float value)
        {
            FloatBits fb = default;
            fb.F = value;
            uint bits = fb.U;

            uint sign = (bits >> 16) & 0x8000u;
            int exp = (int)((bits >> 23) & 0xFFu);
            uint man = bits & 0x007FFFFFu;

            // NaN is CANONICALISED to 0x7E00 rather than sign-preserved. docs/68 §6.4
            // asks for invalid pixels to be "uniformly marked NaN", and .NET's own
            // float.NaN has the sign bit SET (0xFFC00000) — sign-preserving would emit
            // 0xFE00 and make the bytes depend on which NaN the caller happened to hold.
            // A single bit pattern also makes sky regions compress to a constant run.
            if (exp == 0xFF)
                return man != 0 ? NaNBits : (ushort)(sign | 0x7C00u);   // NaN : +/-Inf
            if (exp == 0)                          // binary32 subnormal -> signed zero
                return (ushort)sign;

            int e = exp - 127 + 15;
            if (e >= 0x1F)                         // overflow -> Inf
                return (ushort)(sign | 0x7C00u);

            if (e <= 0)                            // half subnormal or zero
            {
                if (e < -10) return (ushort)sign;
                man |= 0x00800000u;
                int shift = 14 - e;
                uint halfMan = man >> shift;
                uint remMask = (1u << shift) - 1u;
                uint rem = man & remMask;
                uint halfway = 1u << (shift - 1);
                if (rem > halfway || (rem == halfway && (halfMan & 1u) != 0u)) halfMan++;
                return (ushort)(sign | halfMan);
            }

            uint hm = man >> 13;
            uint r = man & 0x1FFFu;
            if (r > 0x1000u || (r == 0x1000u && (hm & 1u) != 0u))
            {
                hm++;
                if (hm == 0x400u)                  // mantissa carry into exponent
                {
                    hm = 0;
                    e++;
                    if (e >= 0x1F) return (ushort)(sign | 0x7C00u);
                }
            }
            return (ushort)(sign | ((uint)e << 10) | hm);
        }

        /// <summary>binary16 bit pattern -> binary32.</summary>
        public static float ToFloat(ushort h)
        {
            uint sign = (uint)((h >> 15) & 1);
            uint exp = (uint)((h >> 10) & 0x1F);
            uint man = (uint)(h & 0x3FF);
            uint bits;

            if (exp == 0x1F)
            {
                bits = (sign << 31) | (0xFFu << 23) | (man << 13);
            }
            else if (exp == 0)
            {
                if (man == 0)
                {
                    bits = sign << 31;
                }
                else                               // half subnormal -> binary32 normal
                {
                    int e = -1;
                    uint m = man;
                    while ((m & 0x400u) == 0) { m <<= 1; e++; }
                    m &= 0x3FFu;
                    bits = (sign << 31) | ((uint)(127 - 15 - e) << 23) | (m << 13);
                }
            }
            else
            {
                bits = (sign << 31) | ((exp - 15 + 127) << 23) | (man << 13);
            }

            FloatBits fb = default;
            fb.U = bits;
            return fb.F;
        }

        /// <summary>Half bit pattern of NaN, the docs/68 invalid-pixel marker.</summary>
        public const ushort NaNBits = 0x7E00;

        /// <summary>True when the bit pattern is a NaN (exponent all ones, non-zero mantissa).</summary>
        public static bool IsNaN(ushort h) => (h & 0x7C00) == 0x7C00 && (h & 0x03FF) != 0;
    }
}
