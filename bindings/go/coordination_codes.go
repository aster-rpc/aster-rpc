package aster

import (
	"crypto/rand"
	"fmt"
)

// Coordination outcome and reason codes (design section 5.2; the same tables
// as the Python and TypeScript bindings) and the rights bits.

// Outcome codes of a MutationReply.
const (
	OutcomeCodeGranted             = 1
	OutcomeCodeRenewed             = 2
	OutcomeCodeMutated             = 3
	OutcomeCodeReleased            = 4
	OutcomeCodeRevoked             = 5
	OutcomeCodeGrantUpdated        = 6
	OutcomeCodeAdminUpdated        = 7
	OutcomeCodeMembershipAccepted  = 8
	OutcomeCodeClientRetired       = 9
	OutcomeCodeMembershipCancelled = 10
	OutcomeCodeRejected            = 100
)

// Reason codes of a rejection (0 = none).
const (
	ReasonCodeNone                   = 0
	ReasonCodeHeldByOther            = 1
	ReasonCodeNotHeld                = 2
	ReasonCodeNotHolder              = 3
	ReasonCodeEpochMismatch          = 4
	ReasonCodeValueRevisionMismatch  = 5
	ReasonCodeACLRevisionMismatch    = 6
	ReasonCodeAdminRevisionMismatch  = 7
	ReasonCodeSequenceTooOld         = 8
	ReasonCodeSequenceGap            = 9
	ReasonCodeSequenceConflict       = 10
	ReasonCodeClientRetired          = 11
	ReasonCodePermissionChanged      = 12
	ReasonCodeMembershipRevMismatch  = 13
	ReasonCodeMembershipIntentActive = 14
	ReasonCodeCounterExhausted       = 15
	ReasonCodeMembershipCannotCancel = 16
	ReasonCodeOperationIDConflict    = 17
	ReasonCodeLastAdministrator      = 24
	ReasonCodeRootAuthorityImmutable = 26
)

// Rights bits of a grant (exact; no wildcards or inheritance).
const (
	RightRead    = 0x01
	RightAcquire = 0x02
	RightRenew   = 0x04
	RightMutate  = 0x08
	RightRelease = 0x10
	RightRevoke  = 0x20
	// RightsHolder is a register holder's set: read, acquire, renew, mutate, release.
	RightsHolder = RightRead | RightAcquire | RightRenew | RightMutate | RightRelease
)

// Rejected reports whether the mutation was a committed typed rejection.
func (m MutationReply) Rejected() bool { return m.OutcomeCode == OutcomeCodeRejected }

// FenceOf is the fence of a held resource snapshot.
func FenceOf(s ResourceSnapshot) (CoordinationFence, bool) {
	if !s.Held || s.Holder == nil {
		return CoordinationFence{}, false
	}
	return CoordinationFence{GroupID: s.GroupID, Resource: s.Resource, Epoch: s.Epoch, Holder: s.Holder}, true
}

// NewOperationID returns a fresh random 16-byte membership operation id.
func NewOperationID() []byte {
	b := make([]byte, 16)
	if _, err := rand.Read(b); err != nil {
		panic(fmt.Sprintf("aster: crypto/rand: %v", err))
	}
	return b
}
