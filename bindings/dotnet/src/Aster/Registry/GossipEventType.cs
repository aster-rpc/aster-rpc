namespace Aster.Registry;

/// <summary>All 6 normative gossip event types (Aster-SPEC.md §11.7).</summary>
public enum GossipEventType
{
    ContractPublished = 0,
    ChannelUpdated = 1,
    EndpointLeaseUpserted = 2,
    EndpointDown = 3,
    AclChanged = 4,
    CompatibilityPublished = 5,
}
