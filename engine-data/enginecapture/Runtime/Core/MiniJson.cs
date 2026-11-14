// Pure C# (no UnityEngine dependency). Minimal JSON reader for the capture SDK's
// plan-side inputs (baked action_camera_N.json / plan.json / covis.json from
// engine-data/trajgen). Parses the full JSON grammar into
//   Dictionary<string, object> / List<object> / string / double / bool / null.
// Numbers are always double (trajgen emits nothing outside double range).
// This is a READER for our own generator's output - it favours clear failure
// (FormatException with position) over leniency.

using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Text;

namespace Diogenes.EngineCapture
{
    /// <summary>Minimal recursive-descent JSON parser (UTF-8 text in, object graph out).</summary>
    public static class MiniJson
    {
        public static object ParseFile(string path)
        {
            return Parse(File.ReadAllText(path, Encoding.UTF8));
        }

        public static object Parse(string text)
        {
            if (text == null) throw new ArgumentNullException(nameof(text));
            int pos = 0;
            object value = ParseValue(text, ref pos);
            SkipWhitespace(text, ref pos);
            if (pos != text.Length)
                throw new FormatException("MiniJson: trailing content at offset " + pos);
            return value;
        }

        static object ParseValue(string s, ref int pos)
        {
            SkipWhitespace(s, ref pos);
            if (pos >= s.Length) throw new FormatException("MiniJson: unexpected end of input");
            char c = s[pos];
            switch (c)
            {
                case '{': return ParseObject(s, ref pos);
                case '[': return ParseArray(s, ref pos);
                case '"': return ParseString(s, ref pos);
                case 't': Expect(s, ref pos, "true"); return true;
                case 'f': Expect(s, ref pos, "false"); return false;
                case 'n': Expect(s, ref pos, "null"); return null;
                default: return ParseNumber(s, ref pos);
            }
        }

        static Dictionary<string, object> ParseObject(string s, ref int pos)
        {
            var obj = new Dictionary<string, object>();
            pos++; // '{'
            SkipWhitespace(s, ref pos);
            if (pos < s.Length && s[pos] == '}') { pos++; return obj; }
            while (true)
            {
                SkipWhitespace(s, ref pos);
                if (pos >= s.Length || s[pos] != '"')
                    throw new FormatException("MiniJson: expected object key at offset " + pos);
                string key = ParseString(s, ref pos);
                SkipWhitespace(s, ref pos);
                if (pos >= s.Length || s[pos] != ':')
                    throw new FormatException("MiniJson: expected ':' at offset " + pos);
                pos++;
                obj[key] = ParseValue(s, ref pos);
                SkipWhitespace(s, ref pos);
                if (pos >= s.Length)
                    throw new FormatException("MiniJson: unterminated object");
                if (s[pos] == ',') { pos++; continue; }
                if (s[pos] == '}') { pos++; return obj; }
                throw new FormatException("MiniJson: expected ',' or '}' at offset " + pos);
            }
        }

        static List<object> ParseArray(string s, ref int pos)
        {
            var arr = new List<object>();
            pos++; // '['
            SkipWhitespace(s, ref pos);
            if (pos < s.Length && s[pos] == ']') { pos++; return arr; }
            while (true)
            {
                arr.Add(ParseValue(s, ref pos));
                SkipWhitespace(s, ref pos);
                if (pos >= s.Length)
                    throw new FormatException("MiniJson: unterminated array");
                if (s[pos] == ',') { pos++; continue; }
                if (s[pos] == ']') { pos++; return arr; }
                throw new FormatException("MiniJson: expected ',' or ']' at offset " + pos);
            }
        }

        static string ParseString(string s, ref int pos)
        {
            var sb = new StringBuilder();
            pos++; // '"'
            while (true)
            {
                if (pos >= s.Length) throw new FormatException("MiniJson: unterminated string");
                char c = s[pos++];
                if (c == '"') return sb.ToString();
                if (c != '\\') { sb.Append(c); continue; }
                if (pos >= s.Length) throw new FormatException("MiniJson: dangling escape");
                char e = s[pos++];
                switch (e)
                {
                    case '"': sb.Append('"'); break;
                    case '\\': sb.Append('\\'); break;
                    case '/': sb.Append('/'); break;
                    case 'b': sb.Append('\b'); break;
                    case 'f': sb.Append('\f'); break;
                    case 'n': sb.Append('\n'); break;
                    case 'r': sb.Append('\r'); break;
                    case 't': sb.Append('\t'); break;
                    case 'u':
                        if (pos + 4 > s.Length) throw new FormatException("MiniJson: bad \\u escape");
                        sb.Append((char)ushort.Parse(s.Substring(pos, 4), NumberStyles.HexNumber,
                            CultureInfo.InvariantCulture));
                        pos += 4;
                        break;
                    default:
                        throw new FormatException("MiniJson: bad escape '\\" + e + "' at offset " + (pos - 1));
                }
            }
        }

        static double ParseNumber(string s, ref int pos)
        {
            int start = pos;
            while (pos < s.Length && ("+-0123456789.eE".IndexOf(s[pos]) >= 0)) pos++;
            if (pos == start)
                throw new FormatException("MiniJson: invalid value at offset " + pos);
            return double.Parse(s.Substring(start, pos - start), NumberStyles.Float,
                CultureInfo.InvariantCulture);
        }

        static void Expect(string s, ref int pos, string token)
        {
            if (pos + token.Length > s.Length || string.CompareOrdinal(s, pos, token, 0, token.Length) != 0)
                throw new FormatException("MiniJson: expected '" + token + "' at offset " + pos);
            pos += token.Length;
        }

        static void SkipWhitespace(string s, ref int pos)
        {
            while (pos < s.Length && (s[pos] == ' ' || s[pos] == '\t' || s[pos] == '\n' || s[pos] == '\r'))
                pos++;
        }
    }
}
