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
