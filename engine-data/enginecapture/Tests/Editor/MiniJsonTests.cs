using System.Collections.Generic;
using System.IO;
using NUnit.Framework;

namespace Diogenes.EngineCapture.Tests
{
    /// <summary>MiniJson（计划侧输入解析）与 BakedPoseTrack 装载的最小回归。</summary>
    public class MiniJsonTests
    {
        [Test]
        public void Parses_Scalars_Arrays_Objects()
        {
            var obj = (Dictionary<string, object>)MiniJson.Parse(
                "{\"a\": 1.5, \"b\": [1, -2e2, 3], \"c\": {\"d\": \"x\\ny\"}, \"e\": true, \"f\": null}");
            Assert.AreEqual(1.5, (double)obj["a"], 1e-12);
            var arr = (List<object>)obj["b"];
            Assert.AreEqual(-200.0, (double)arr[1], 1e-12);
            Assert.AreEqual("x\ny", ((Dictionary<string, object>)obj["c"])["d"]);
            Assert.AreEqual(true, obj["e"]);
            Assert.IsNull(obj["f"]);
        }

        [Test]
        public void Rejects_Trailing_Garbage()
        {
            Assert.Throws<System.FormatException>(() => MiniJson.Parse("{} x"));
        }

        [Test]
        public void BakedPoseTrack_Loads_And_Validates_Frame_Numbers()
        {
            string path = Path.Combine(Path.GetTempPath(), "diogenes-minijson-test.json");
            File.WriteAllText(path,
                "[{\"frame\":0,\"camera_position\":[1,2,3],\"camera_rotation_quaternion\":[0,0,0,1]}," +
                "{\"frame\":1,\"camera_position\":[4,5,6],\"camera_rotation_quaternion\":[0,0.7071,0,0.7071]}]");
            var track = BakedPoseTrack.Load(path);
            Assert.AreEqual(2, track.FrameCount);
            Assert.AreEqual(5.0, track.Positions[1][1], 1e-12);
            Assert.AreEqual(0.7071, track.Quaternions[1][3], 1e-12);

            File.WriteAllText(path,
                "[{\"frame\":7,\"camera_position\":[1,2,3],\"camera_rotation_quaternion\":[0,0,0,1]}]");
            Assert.Throws<InvalidDataException>(() => BakedPoseTrack.Load(path));
            File.Delete(path);
        }
    }
}
