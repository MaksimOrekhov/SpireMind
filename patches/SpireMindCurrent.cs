using System;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Threading;

namespace SpireMentor;

// The generated schema is also compiled into the Rust receiver. A new
// field needs an explicit declaration even when upstream adds it to a DTO.
internal static class SpireMindCurrent
{
    internal const int Version = 1;
    private static readonly JsonElement Schema = JsonSerializer.Deserialize<JsonElement>(SpireMindCurrentSchema.Json);
    private static readonly ConditionalWeakTable<object, CardHandle> HandHandles = new();
    private static long _nextHandle;

    private sealed record CardHandle(long Id);

    // Assign on first appearance in the hand, never while traversing a draw pile.
    internal static long HandInstance(object card) =>
        HandHandles.GetValue(card, _ => new CardHandle(Interlocked.Increment(ref _nextHandle))).Id;

    internal static JsonObject Combat(Dictionary<string, object> state)
    {
        var source = JsonSerializer.SerializeToNode(state)!.AsObject();
        foreach (JsonNode? entry in source["enemies"]!.AsArray())
        {
            JsonObject enemy = entry!.AsObject();
            // NextMove in the pinned game denotes the already rolled,
            // currently telegraphed move. Its FollowUpStateId is excluded.
            enemy["current_move"] = enemy["next_move_id"]?.DeepClone();
            if (enemy["ai"] is JsonObject ai)
                enemy["completed_move_flags"] = ai.DeepClone();
        }
        JsonObject frame = Project(source, Schema.GetProperty("frame"))!.AsObject();
        frame["native_current"] = Version;
        return frame;
    }

    internal static string Key(JsonObject frame)
    {
        var position = (JsonObject)frame.DeepClone();
        position.Remove("state_id");
        position.Remove("request_id");
        position.Remove("capabilities");
        return Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(position.ToJsonString())));
    }

    internal static string Screen(Dictionary<string, object> state)
    {
        foreach (string field in new[] { "seed", "rng", "coop_rng", "combat_log" })
            state.Remove(field);
        if (state.Remove("combat"))
            state.TryAdd("surface", "embedded_combat");
        return JsonSerializer.Serialize(state);
    }

    private static JsonNode? Project(JsonNode? value, JsonElement shape)
    {
        if (value is null)
            return null;
        if (shape.ValueKind == JsonValueKind.String)
        {
            string name = shape.GetString()!;
            switch (name)
            {
                case "s" when value.GetValueKind() == JsonValueKind.String:
                case "i" when value is JsonValue n && n.TryGetValue(out long _):
                case "b" when value.GetValueKind() is JsonValueKind.True or JsonValueKind.False:
                    return value.DeepClone();
                case "s": case "i": case "b":
                    throw new JsonException("Invalid native current-state scalar");
                default:
                    return Project(value, Schema.GetProperty(name));
            }
        }
        if (shape.ValueKind == JsonValueKind.Array)
        {
            IEnumerable<JsonNode?> entries = value.AsArray().Select(row =>
                Project(row, shape[0]) ?? throw new JsonException("Null current-state list entry"));
            if (shape.GetArrayLength() == 2)
                entries = entries.OrderBy(row => row!.ToJsonString(), StringComparer.Ordinal);
            return new JsonArray(entries.ToArray());
        }
        var result = new JsonObject();
        foreach (JsonProperty field in shape.EnumerateObject())
            if (value.AsObject().TryGetPropertyValue(field.Name, out JsonNode? item))
                result.Add(field.Name, Project(item, field.Value));
        return result;
    }
}
