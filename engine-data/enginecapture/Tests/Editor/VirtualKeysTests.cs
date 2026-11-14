// EditMode tests (Unity Test Runner). NOT yet executed - this repo has no Unity
// toolchain; run via Window > General > Test Runner (EditMode) after import.

using NUnit.Framework;

namespace Diogenes.EngineCapture.Tests
{
    public class VirtualKeysTests
    {
        // Spot checks against the demand-side ASCII VK table (docs/69; W=87 appears in
        // the delivery sample's keyCode array).
        [TestCase("W", 87)]
        [TestCase("A", 65)]
        [TestCase("S", 83)]
        [TestCase("D", 68)]
        [TestCase("Space", 32)]
        [TestCase("Shift", 16)]
        [TestCase("Control", 17)]
        [TestCase("Alt", 18)]
        [TestCase("Escape", 27)]
        [TestCase("Left", 37)]
        [TestCase("Up", 38)]
        [TestCase("Right", 39)]
        [TestCase("Down", 40)]
        [TestCase("LMB", 1)]
        [TestCase("RMB", 2)]
        [TestCase("MMB", 4)]
        public void ByName_MatchesVkTable(string name, int expected)
        {
            Assert.That(VirtualKeys.ByName[name], Is.EqualTo(expected));
        }

        [Test]
        public void Letters_AreAscii()
        {
            Assert.That(VirtualKeys.Letter('A'), Is.EqualTo(65));
            Assert.That(VirtualKeys.Letter('z'), Is.EqualTo(90)); // case-insensitive
            Assert.That(VirtualKeys.Letter('w'), Is.EqualTo(VirtualKeys.W));
        }

        [Test]
        public void Digits_AreAscii()
        {
            Assert.That(VirtualKeys.Digit(0), Is.EqualTo(48));
            Assert.That(VirtualKeys.Digit(9), Is.EqualTo(57));
        }
    }
}
