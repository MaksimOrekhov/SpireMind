using System.Text.Json;
using SpireMentor;

static void Check(bool condition, string name)
{
    if (!condition)
        throw new InvalidOperationException(name);
}

static JsonElement Read(string json) => JsonSerializer.Deserialize<JsonElement>(json);

Check(SpireMindLease.OptedIn(Read("""{"capabilities":{"spiremind_lease":1}}""")), "explicit lease opt-in");
Check(!SpireMindLease.OptedIn(Read("""{"capabilities":{"spiremind_lease":2}}""")), "unknown protocol");
Check(!SpireMindLease.OptedIn(Read("""{"capabilities":{"spiremind_lease":"1"}}""")), "invalid protocol");
Check(!SpireMindLease.OptedIn(Read("""{"action":"ping"}""")), "ordinary heartbeat");
var screen = new object();
var lease = new SpireMindLease(3, "a8", 20, 9, screen, "map:rows");
for (int i = 1; i <= 24; i++)
{
    var frame = Read(lease.Confirm(3, 9, screen, "map:rows") ?? throw new Exception("missing pulse"));
    Check(frame.GetProperty("type").GetString() == "spiremind_confirm", "message type");
    Check(frame.GetProperty("request_id").GetString() == "a8", "request identity");
    Check(frame.GetProperty("state_id").GetInt32() == 20, "state identity");
    Check(frame.GetProperty("lease_seq").GetInt32() == i, "monotonic sequence");
}
Check(lease.Confirm(4, 9, screen, "map:rows") is null, "connection changed");
Check(lease.Confirm(3, 10, screen, "map:rows") is null, "board changed");
Check(lease.Confirm(3, 9, new object(), "map:rows") is null, "screen replaced");
Check(lease.Confirm(3, 9, screen, "map:other") is null, "visible options changed");
Check(lease.Confirm(3, 9, null, null) is null, "screen closed or hidden");
var withdrawal = Read(lease.Withdraw());
Check(withdrawal.GetProperty("type").GetString() == "spiremind_withdraw", "withdrawal");
Check(withdrawal.GetProperty("state_id").GetInt32() == 20, "withdrawn identity");
Check(lease.Confirm(3, 9, screen, "map:rows") is null, "withdrawal cannot be revived");
var combat = new SpireMindLease(3, "a9", 21, 12, null, "combat_action");
Check(combat.Confirm(3, 12, null, "combat_action") is not null, "settled combat");
Check(combat.Confirm(3, 13, null, "combat_action") is null, "combat epoch changed with the screen still open");
Check(combat.Confirm(3, 12, null, null) is null, "combat actions disabled");
Console.WriteLine("Observer lease checks passed");

const string input = """
    {"type":"advice_request","state_id":42,"request_id":"a42","seed":"hidden",
     "rng":{"run_seed":17},"run_state":{"gold":5,"future_rewards":["hidden"]},
     "player":{"energy":3,"powers":[{"id":"CRIMSON_MANTLE","vars":{"SelfDamage":2,"rng":99}}]},
     "hand":[{"id":"BASH","combat_vars":{"return_if_played_round":2,"future_result":99}}],
     "draw_pile":{"count":2,"cards":[{"id":"BASH","uid":17,"stable_idx":1},{"id":"ANGER","uid":28,"stable_idx":2}],"pinned_top":[17]},
     "enemies":[{"id":"NIBBIT","next_move_id":"CURRENT","forecast":["hidden"],
                 "ai":{"performed":true,"forced_next":"hidden","log":["hidden"]},
                 "intents":[{"intent_type":"Attack","damage":5,"forecast":"hidden"}]}]}
    """;
var frameA = SpireMindCurrent.Combat(JsonSerializer.Deserialize<Dictionary<string, object>>(input)!);
var frameB = SpireMindCurrent.Combat(JsonSerializer.Deserialize<Dictionary<string, object>>(
    input.Replace("hidden", "changed").Replace("a42", "a43").Replace(":42", ":43")
        .Replace("""{"id":"BASH","uid":17,"stable_idx":1},{"id":"ANGER","uid":28,"stable_idx":2}""",
                 """{"id":"ANGER","uid":111,"stable_idx":1},{"id":"BASH","uid":222,"stable_idx":2}"""))!);
Check(frameA["native_current"]!.GetValue<int>() == 1, "native contract version");
Check(frameA["enemies"]![0]!["current_move"]!.GetValue<string>() == "CURRENT", "current rolled move preserved");
Check(frameA["enemies"]![0]!["completed_move_flags"]!["performed"]!.GetValue<bool>(), "completed move flag preserved");
Check(frameA["player"]!["powers"]![0]!["vars"]!["SelfDamage"]!.GetValue<int>() == 2, "current counter preserved");
Check(frameA["hand"]![0]!["combat_vars"]!["return_if_played_round"]!.GetValue<int>() == 2, "current card parameter preserved");
var firstCard = new object();
var secondCard = new object();
Check(SpireMindCurrent.HandInstance(firstCard) == SpireMindCurrent.HandInstance(firstCard), "same card instance");
Check(SpireMindCurrent.HandInstance(firstCard) != SpireMindCurrent.HandInstance(secondCard), "different card instances");
string admitted = frameA.ToJsonString();
foreach (string forbidden in new[] { "hidden", "rng", "uid", "stable_idx", "pinned_top", "future_result", "forced_next", "forecast" })
    Check(!admitted.Contains(forbidden), $"native frame excludes {forbidden}");
string key = SpireMindCurrent.Key(frameA);
Check(SpireMindCurrent.Key(frameB) == key, "future changes and new correlation do not alter confirmation");
var nativeLease = new SpireMindLease(4, "a42", 42, 18, null, key);
for (int i = 0; i < 22; i++)
    Check(nativeLease.Confirm(4, 18, null, SpireMindCurrent.Key(frameB)) is not null, "unchanged native combat pulse");
frameB["player"]!["powers"]![0]!["vars"]!["SelfDamage"] = 3;
Check(nativeLease.Confirm(4, 18, null, SpireMindCurrent.Key(frameB)) is null, "changed current counter invalidates advice");
Console.WriteLine("Native current-state producer checks passed");
