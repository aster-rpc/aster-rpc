//go:build cgo

package aster

import (
	"fmt"
	"log"
	"math"
	"math/rand"
	"strings"
	"sync"
	"time"
)

// ---------------------------------------------------------------------------
// StatusCode -- RPC status codes (gRPC-mirrored 0-16, Aster-native 100+)
// ---------------------------------------------------------------------------

// StatusCode represents an Aster RPC status code.
// Codes 0-16 mirror gRPC semantics; codes 100+ are Aster-native.
type StatusCode int

const (
	StatusOK                 StatusCode = 0
	StatusCancelled          StatusCode = 1
	StatusUnknown            StatusCode = 2
	StatusInvalidArgument    StatusCode = 3
	StatusDeadlineExceeded   StatusCode = 4
	StatusNotFound           StatusCode = 5
	StatusAlreadyExists      StatusCode = 6
	StatusPermissionDenied   StatusCode = 7
	StatusResourceExhausted  StatusCode = 8
	StatusFailedPrecondition StatusCode = 9
	StatusAborted            StatusCode = 10
	StatusOutOfRange         StatusCode = 11
	StatusUnimplemented      StatusCode = 12
	StatusInternal           StatusCode = 13
	StatusUnavailable        StatusCode = 14
	StatusDataLoss           StatusCode = 15
	StatusUnauthenticated    StatusCode = 16
	StatusContractViolation  StatusCode = 101
)

var statusCodeNames = map[StatusCode]string{
	StatusOK:                 "OK",
	StatusCancelled:          "CANCELLED",
	StatusUnknown:            "UNKNOWN",
	StatusInvalidArgument:    "INVALID_ARGUMENT",
	StatusDeadlineExceeded:   "DEADLINE_EXCEEDED",
	StatusNotFound:           "NOT_FOUND",
	StatusAlreadyExists:      "ALREADY_EXISTS",
	StatusPermissionDenied:   "PERMISSION_DENIED",
	StatusResourceExhausted:  "RESOURCE_EXHAUSTED",
	StatusFailedPrecondition: "FAILED_PRECONDITION",
	StatusAborted:            "ABORTED",
	StatusOutOfRange:         "OUT_OF_RANGE",
	StatusUnimplemented:      "UNIMPLEMENTED",
	StatusInternal:           "INTERNAL",
	StatusUnavailable:        "UNAVAILABLE",
	StatusDataLoss:           "DATA_LOSS",
	StatusUnauthenticated:    "UNAUTHENTICATED",
	StatusContractViolation:  "CONTRACT_VIOLATION",
}

// String returns the human-readable name for the status code.
func (c StatusCode) String() string {
	if name, ok := statusCodeNames[c]; ok {
		return name
	}
	return fmt.Sprintf("STATUS_%d", int(c))
}

// ---------------------------------------------------------------------------
// RpcError
// ---------------------------------------------------------------------------

// RpcError is the error type returned when an RPC call fails.
type RpcError struct {
	Code    StatusCode
	Message string
}

func (e *RpcError) Error() string {
	return fmt.Sprintf("[%s] %s", e.Code, e.Message)
}

// ---------------------------------------------------------------------------
// CallContext
// ---------------------------------------------------------------------------

// CallContext carries per-call metadata through the interceptor chain.
type CallContext struct {
	Service    string
	Method     string
	CallID     string
	SessionID  string
	Peer       string
	Metadata   map[string]string
	Attributes map[string]string
	Deadline   *time.Time
	IsStreaming bool
	Pattern    string
	Idempotent bool
	Attempt    int
}

// RemainingSeconds returns the seconds until the deadline, or nil if no
// deadline is set.
func (c *CallContext) RemainingSeconds() *float64 {
	if c.Deadline == nil {
		return nil
	}
	remaining := time.Until(*c.Deadline).Seconds()
	if remaining < 0 {
		remaining = 0
	}
	return &remaining
}

// IsExpired returns true when the deadline has passed.
func (c *CallContext) IsExpired() bool {
	rem := c.RemainingSeconds()
	return rem != nil && *rem <= 0
}

// ---------------------------------------------------------------------------
// Interceptor interface
// ---------------------------------------------------------------------------

// Interceptor is the base interface for all Aster RPC interceptors.
// Each method has a default pass-through behaviour -- implementors need
// only override the hooks they care about.
type Interceptor interface {
	// OnRequest is called before the handler executes.
	// Return the (possibly modified) request, or an error to abort.
	OnRequest(ctx *CallContext, request any) (any, error)

	// OnResponse is called after the handler returns successfully.
	// Return the (possibly modified) response, or an error to abort.
	OnResponse(ctx *CallContext, response any) (any, error)

	// OnError is called when an RPC error occurs.
	// Return nil to swallow the error, or a (possibly modified) *RpcError.
	OnError(ctx *CallContext, err *RpcError) *RpcError
}

// BaseInterceptor provides no-op defaults so concrete interceptors only
// need to embed it and override the methods they use.
type BaseInterceptor struct{}

func (BaseInterceptor) OnRequest(_ *CallContext, request any) (any, error) { return request, nil }
func (BaseInterceptor) OnResponse(_ *CallContext, response any) (any, error) {
	return response, nil
}
func (BaseInterceptor) OnError(_ *CallContext, err *RpcError) *RpcError { return err }

// ---------------------------------------------------------------------------
// Chain execution helpers
// ---------------------------------------------------------------------------

// ApplyRequestInterceptors runs OnRequest through interceptors in order.
// Stops and returns the first error encountered.
func ApplyRequestInterceptors(interceptors []Interceptor, ctx *CallContext, request any) (any, error) {
	current := request
	for _, ic := range interceptors {
		var err error
		current, err = ic.OnRequest(ctx, current)
		if err != nil {
			return nil, err
		}
	}
	return current, nil
}

// ApplyResponseInterceptors runs OnResponse through interceptors in order.
// Stops and returns the first error encountered.
func ApplyResponseInterceptors(interceptors []Interceptor, ctx *CallContext, response any) (any, error) {
	current := response
	for _, ic := range interceptors {
		var err error
		current, err = ic.OnResponse(ctx, current)
		if err != nil {
			return nil, err
		}
	}
	return current, nil
}

// ApplyErrorInterceptors runs OnError through interceptors in reverse order.
// Returns nil if any interceptor swallows the error.
func ApplyErrorInterceptors(interceptors []Interceptor, ctx *CallContext, rpcErr *RpcError) *RpcError {
	current := rpcErr
	for i := len(interceptors) - 1; i >= 0; i-- {
		if current == nil {
			return nil
		}
		current = interceptors[i].OnError(ctx, current)
	}
	return current
}

// ---------------------------------------------------------------------------
// DeadlineInterceptor
// ---------------------------------------------------------------------------

// DeadlineInterceptor validates and enforces call deadlines.
// SkewToleranceMs is the milliseconds of clock-skew tolerance added to the
// deadline when checking on receipt (default 5000).
type DeadlineInterceptor struct {
	BaseInterceptor
	SkewToleranceMs int
}

// NewDeadlineInterceptor creates a DeadlineInterceptor with sensible defaults.
func NewDeadlineInterceptor() *DeadlineInterceptor {
	return &DeadlineInterceptor{SkewToleranceMs: 5000}
}

func (d *DeadlineInterceptor) OnRequest(ctx *CallContext, request any) (any, error) {
	if ctx.Deadline == nil {
		return request, nil
	}
	now := time.Now()
	dl := *ctx.Deadline
	nowMs := now.UnixMilli()
	dlMs := dl.UnixMilli()
	tolerance := int64(d.SkewToleranceMs)

	// Reject on receipt if expired beyond skew tolerance.
	if nowMs > dlMs+tolerance {
		return nil, &RpcError{
			Code: StatusDeadlineExceeded,
			Message: fmt.Sprintf(
				"deadline already expired on receipt (now=%d, deadline=%d, skew_tolerance=%dms)",
				nowMs, dlMs, d.SkewToleranceMs,
			),
		}
	}
	// Standard expiry check (no tolerance).
	if ctx.IsExpired() {
		return nil, &RpcError{Code: StatusDeadlineExceeded, Message: "deadline exceeded"}
	}
	return request, nil
}

// TimeoutSeconds returns the remaining seconds until the deadline, or nil
// if no deadline is set.
func (d *DeadlineInterceptor) TimeoutSeconds(ctx *CallContext) *float64 {
	rem := ctx.RemainingSeconds()
	if rem == nil {
		return nil
	}
	v := math.Max(0, *rem)
	return &v
}

// ---------------------------------------------------------------------------
// AuthInterceptor
// ---------------------------------------------------------------------------

// TokenProvider is either a static string or a function returning a string.
type TokenProvider interface {
	Token() string
}

type staticToken string

func (s staticToken) Token() string { return string(s) }

// StaticToken returns a TokenProvider that always returns the same token.
func StaticToken(tok string) TokenProvider { return staticToken(tok) }

type funcToken func() string

func (f funcToken) Token() string { return f() }

// FuncToken returns a TokenProvider backed by a function.
func FuncToken(fn func() string) TokenProvider { return funcToken(fn) }

// TokenValidator is either a static string (exact match) or a function.
type TokenValidator interface {
	Validate(token string) bool
}

type staticValidator string

func (s staticValidator) Validate(tok string) bool { return tok == string(s) }

// StaticValidator returns a TokenValidator that checks for exact match.
func StaticValidator(expected string) TokenValidator { return staticValidator(expected) }

type funcValidator func(string) bool

func (f funcValidator) Validate(tok string) bool { return f(tok) }

// FuncValidator returns a TokenValidator backed by a function.
func FuncValidator(fn func(string) bool) TokenValidator { return funcValidator(fn) }

// AuthInterceptor injects and/or validates auth metadata.
type AuthInterceptor struct {
	BaseInterceptor
	Provider    TokenProvider
	Validator   TokenValidator
	MetadataKey string // default "authorization"
	Scheme      string // default "Bearer", empty string to disable
}

// NewAuthInterceptor creates an AuthInterceptor with sensible defaults.
func NewAuthInterceptor() *AuthInterceptor {
	return &AuthInterceptor{
		MetadataKey: "authorization",
		Scheme:      "Bearer",
	}
}

func (a *AuthInterceptor) OnRequest(ctx *CallContext, request any) (any, error) {
	key := a.MetadataKey
	if key == "" {
		key = "authorization"
	}

	// Inject token if provider is set and the key is not already present.
	if a.Provider != nil {
		if _, exists := ctx.Metadata[key]; !exists {
			tok := a.Provider.Token()
			if a.Scheme != "" {
				ctx.Metadata[key] = a.Scheme + " " + tok
			} else {
				ctx.Metadata[key] = tok
			}
		}
	}

	// Validate token if validator is set.
	if a.Validator != nil {
		rawToken := ctx.Metadata[key]
		token := rawToken
		if a.Scheme != "" {
			prefix := a.Scheme + " "
			if strings.HasPrefix(rawToken, prefix) {
				token = rawToken[len(prefix):]
			}
		}
		if !a.Validator.Validate(token) {
			return nil, &RpcError{Code: StatusUnauthenticated, Message: "authentication failed"}
		}
	}

	return request, nil
}

// ---------------------------------------------------------------------------
// RateLimitInterceptor
// ---------------------------------------------------------------------------

// tokenBucket is a simple token-bucket rate limiter.
type tokenBucket struct {
	mu         sync.Mutex
	rate       float64   // tokens per second
	capacity   float64   // max burst
	tokens     float64
	lastRefill time.Time
}

func newTokenBucket(rate, burst float64) *tokenBucket {
	if burst <= 0 {
		burst = rate
	}
	return &tokenBucket{
		rate:       rate,
		capacity:   burst,
		tokens:     burst,
		lastRefill: time.Now(),
	}
}

func (b *tokenBucket) tryAcquire() bool {
	b.mu.Lock()
	defer b.mu.Unlock()

	now := time.Now()
	elapsed := now.Sub(b.lastRefill).Seconds()
	b.tokens = math.Min(b.capacity, b.tokens+elapsed*b.rate)
	b.lastRefill = now

	if b.tokens >= 1.0 {
		b.tokens -= 1.0
		return true
	}
	return false
}

// RateLimitInterceptor enforces a token-bucket rate limit.
// Per controls the granularity: "global", "service", "method", or "peer".
type RateLimitInterceptor struct {
	BaseInterceptor
	Rate  float64 // tokens per second (default 100)
	Burst float64 // max burst (defaults to Rate)
	Per   string  // "global" (default), "service", "method", "peer"

	mu           sync.Mutex
	globalBucket *tokenBucket
	buckets      map[string]*tokenBucket
}

// NewRateLimitInterceptor creates a RateLimitInterceptor with sensible defaults.
func NewRateLimitInterceptor(rate float64) *RateLimitInterceptor {
	return &RateLimitInterceptor{
		Rate: rate,
		Per:  "global",
	}
}

func (r *RateLimitInterceptor) getBucket(ctx *CallContext) *tokenBucket {
	r.mu.Lock()
	defer r.mu.Unlock()

	// Lazily initialise the global bucket.
	if r.globalBucket == nil {
		burst := r.Burst
		if burst <= 0 {
			burst = r.Rate
		}
		r.globalBucket = newTokenBucket(r.Rate, burst)
	}

	if r.Per == "" || r.Per == "global" {
		return r.globalBucket
	}

	var key string
	switch r.Per {
	case "service":
		key = ctx.Service
	case "method":
		key = ctx.Service + "." + ctx.Method
	case "peer":
		key = ctx.Peer
		if key == "" {
			key = "unknown"
		}
	default:
		return r.globalBucket
	}

	if r.buckets == nil {
		r.buckets = make(map[string]*tokenBucket)
	}
	if _, ok := r.buckets[key]; !ok {
		burst := r.Burst
		if burst <= 0 {
			burst = r.Rate
		}
		r.buckets[key] = newTokenBucket(r.Rate, burst)
	}
	return r.buckets[key]
}

func (r *RateLimitInterceptor) OnRequest(ctx *CallContext, request any) (any, error) {
	bucket := r.getBucket(ctx)
	if !bucket.tryAcquire() {
		per := r.Per
		if per == "" {
			per = "global"
		}
		return nil, &RpcError{
			Code:    StatusResourceExhausted,
			Message: fmt.Sprintf("Rate limit exceeded (%.0f/s per %s)", r.Rate, per),
		}
	}
	return request, nil
}

// ---------------------------------------------------------------------------
// CapabilityInterceptor
// ---------------------------------------------------------------------------

// CapabilityInterceptor enforces role-based access control.
// ServiceRoles maps "ServiceName" or "ServiceName.MethodName" to the list
// of roles that are permitted to call it. The caller's role is read from
// ctx.Attributes["role"].
type CapabilityInterceptor struct {
	BaseInterceptor
	// ServiceRoles maps a service or "service.method" key to the permitted roles.
	ServiceRoles map[string][]string
}

// NewCapabilityInterceptor creates a CapabilityInterceptor.
func NewCapabilityInterceptor(roles map[string][]string) *CapabilityInterceptor {
	return &CapabilityInterceptor{ServiceRoles: roles}
}

func (c *CapabilityInterceptor) OnRequest(ctx *CallContext, request any) (any, error) {
	callerRole := ctx.Attributes["role"]

	// Check service-level requirement.
	if allowed, ok := c.ServiceRoles[ctx.Service]; ok {
		if !containsRole(allowed, callerRole) {
			return nil, &RpcError{
				Code:    StatusPermissionDenied,
				Message: fmt.Sprintf("capability check failed for service '%s'", ctx.Service),
			}
		}
	}

	// Check method-level requirement.
	methodKey := ctx.Service + "." + ctx.Method
	if allowed, ok := c.ServiceRoles[methodKey]; ok {
		if !containsRole(allowed, callerRole) {
			return nil, &RpcError{
				Code:    StatusPermissionDenied,
				Message: fmt.Sprintf("capability check failed for method '%s'", methodKey),
			}
		}
	}

	return request, nil
}

func containsRole(allowed []string, role string) bool {
	for _, r := range allowed {
		if r == role {
			return true
		}
	}
	return false
}

// ---------------------------------------------------------------------------
// AuditInterceptor
// ---------------------------------------------------------------------------

// AuditInterceptor records structured audit events for requests, responses,
// and errors to an in-memory sink and a log.Logger.
type AuditInterceptor struct {
	BaseInterceptor
	mu     sync.Mutex
	Sink   []map[string]any
	Logger *log.Logger
}

// NewAuditInterceptor creates an AuditInterceptor.
// If logger is nil, the default log.Logger is used.
func NewAuditInterceptor(logger *log.Logger) *AuditInterceptor {
	if logger == nil {
		logger = log.Default()
	}
	return &AuditInterceptor{
		Sink:   make([]map[string]any, 0),
		Logger: logger,
	}
}

func (a *AuditInterceptor) record(event string, ctx *CallContext, extra map[string]any) {
	entry := map[string]any{
		"event":   event,
		"service": ctx.Service,
		"method":  ctx.Method,
		"call_id": ctx.CallID,
		"attempt": ctx.Attempt,
		"ts":      time.Now().UnixMilli(),
	}
	for k, v := range extra {
		entry[k] = v
	}
	a.mu.Lock()
	a.Sink = append(a.Sink, entry)
	a.mu.Unlock()
	a.Logger.Printf("audit=%v", entry)
}

func (a *AuditInterceptor) OnRequest(ctx *CallContext, request any) (any, error) {
	a.record("request", ctx, nil)
	return request, nil
}

func (a *AuditInterceptor) OnResponse(ctx *CallContext, response any) (any, error) {
	a.record("response", ctx, nil)
	return response, nil
}

func (a *AuditInterceptor) OnError(ctx *CallContext, err *RpcError) *RpcError {
	a.record("error", ctx, map[string]any{
		"code":    err.Code.String(),
		"message": err.Message,
	})
	return err
}

// Events returns a snapshot of the audit log.
func (a *AuditInterceptor) Events() []map[string]any {
	a.mu.Lock()
	defer a.mu.Unlock()
	out := make([]map[string]any, len(a.Sink))
	copy(out, a.Sink)
	return out
}

// ---------------------------------------------------------------------------
// CircuitBreakerInterceptor
// ---------------------------------------------------------------------------

// Circuit breaker states.
const (
	circuitClosed   = "closed"
	circuitOpen     = "open"
	circuitHalfOpen = "half_open"
)

// CircuitBreakerInterceptor implements a CLOSED -> OPEN -> HALF_OPEN circuit
// breaker. It tracks consecutive failures and opens the circuit when the
// threshold is reached.
type CircuitBreakerInterceptor struct {
	BaseInterceptor
	FailureThreshold  int     // failures before opening (default 3)
	RecoveryTimeout   float64 // seconds before transitioning to half-open (default 5.0)
	HalfOpenMaxCalls  int     // max calls allowed in half-open state (default 1)

	mu             sync.Mutex
	state          string
	failureCount   int
	openedAt       time.Time
	halfOpenCalls  int
}

// NewCircuitBreakerInterceptor creates a CircuitBreakerInterceptor with
// sensible defaults.
func NewCircuitBreakerInterceptor() *CircuitBreakerInterceptor {
	return &CircuitBreakerInterceptor{
		FailureThreshold: 3,
		RecoveryTimeout:  5.0,
		HalfOpenMaxCalls: 1,
		state:            circuitClosed,
	}
}

// State returns the current circuit state.
func (cb *CircuitBreakerInterceptor) State() string {
	cb.mu.Lock()
	defer cb.mu.Unlock()
	return cb.state
}

// BeforeCall checks the circuit state and rejects if open.
// Called from OnRequest.
func (cb *CircuitBreakerInterceptor) beforeCall() error {
	now := time.Now()

	if cb.state == circuitOpen {
		elapsed := now.Sub(cb.openedAt).Seconds()
		if elapsed >= cb.RecoveryTimeout {
			cb.state = circuitHalfOpen
			cb.halfOpenCalls = 0
		} else {
			return &RpcError{Code: StatusUnavailable, Message: "circuit breaker is open"}
		}
	}

	if cb.state == circuitHalfOpen {
		if cb.halfOpenCalls >= cb.HalfOpenMaxCalls {
			return &RpcError{Code: StatusUnavailable, Message: "circuit breaker is half-open"}
		}
		cb.halfOpenCalls++
	}
	return nil
}

// RecordSuccess resets the breaker to closed.
func (cb *CircuitBreakerInterceptor) RecordSuccess() {
	cb.mu.Lock()
	defer cb.mu.Unlock()
	cb.failureCount = 0
	cb.halfOpenCalls = 0
	cb.state = circuitClosed
}

// RecordFailure records a failure and may trip the breaker open.
func (cb *CircuitBreakerInterceptor) RecordFailure(err *RpcError) {
	cb.mu.Lock()
	defer cb.mu.Unlock()
	cb.recordFailure(err)
}

func (cb *CircuitBreakerInterceptor) recordFailure(err *RpcError) {
	// Only count transient server-side errors.
	switch err.Code {
	case StatusUnavailable, StatusInternal, StatusUnknown:
	default:
		return
	}

	if cb.state == circuitHalfOpen {
		cb.state = circuitOpen
		cb.openedAt = time.Now()
		cb.halfOpenCalls = 0
		return
	}

	cb.failureCount++
	if cb.failureCount >= cb.FailureThreshold {
		cb.state = circuitOpen
		cb.openedAt = time.Now()
	}
}

func (cb *CircuitBreakerInterceptor) OnRequest(ctx *CallContext, request any) (any, error) {
	cb.mu.Lock()
	defer cb.mu.Unlock()
	if err := cb.beforeCall(); err != nil {
		return nil, err
	}
	return request, nil
}

func (cb *CircuitBreakerInterceptor) OnResponse(ctx *CallContext, response any) (any, error) {
	cb.RecordSuccess()
	return response, nil
}

func (cb *CircuitBreakerInterceptor) OnError(ctx *CallContext, err *RpcError) *RpcError {
	cb.mu.Lock()
	defer cb.mu.Unlock()
	cb.recordFailure(err)
	return err
}

// ---------------------------------------------------------------------------
// CompressionInterceptor
// ---------------------------------------------------------------------------

// DefaultCompressionThreshold is the default payload size (bytes) above
// which compression is applied (matches the Python default of 4096).
const DefaultCompressionThreshold = 4096

// CompressionInterceptor injects compression configuration into call
// metadata so downstream transport code can honour it.
type CompressionInterceptor struct {
	BaseInterceptor
	Threshold int  // payload size threshold in bytes (-1 to disable)
	Enabled   bool // master switch
}

// NewCompressionInterceptor creates a CompressionInterceptor with defaults.
func NewCompressionInterceptor() *CompressionInterceptor {
	return &CompressionInterceptor{
		Threshold: DefaultCompressionThreshold,
		Enabled:   true,
	}
}

func (c *CompressionInterceptor) OnRequest(ctx *CallContext, request any) (any, error) {
	threshold := c.Threshold
	if !c.Enabled {
		threshold = -1
	}
	ctx.Metadata["_aster_compress_threshold"] = fmt.Sprintf("%d", threshold)
	ctx.Metadata["_aster_compress_enabled"] = fmt.Sprintf("%t", c.Enabled)
	return request, nil
}

// ---------------------------------------------------------------------------
// MetricsInterceptor
// ---------------------------------------------------------------------------

// MetricsInterceptor collects RED metrics (Rate, Errors, Duration) with
// simple in-memory counters. No external dependency required.
type MetricsInterceptor struct {
	BaseInterceptor

	mu        sync.Mutex
	started   int64
	succeeded int64
	failed    int64

	callStarts map[string]time.Time
}

// NewMetricsInterceptor creates a MetricsInterceptor.
func NewMetricsInterceptor() *MetricsInterceptor {
	return &MetricsInterceptor{
		callStarts: make(map[string]time.Time),
	}
}

func (m *MetricsInterceptor) callKey(ctx *CallContext) string {
	return fmt.Sprintf("%s.%s.%s", ctx.Service, ctx.Method, ctx.CallID)
}

func (m *MetricsInterceptor) OnRequest(ctx *CallContext, request any) (any, error) {
	m.mu.Lock()
	m.started++
	m.callStarts[m.callKey(ctx)] = time.Now()
	m.mu.Unlock()
	return request, nil
}

func (m *MetricsInterceptor) OnResponse(ctx *CallContext, response any) (any, error) {
	m.mu.Lock()
	m.succeeded++
	delete(m.callStarts, m.callKey(ctx))
	m.mu.Unlock()
	return response, nil
}

func (m *MetricsInterceptor) OnError(ctx *CallContext, err *RpcError) *RpcError {
	m.mu.Lock()
	m.failed++
	delete(m.callStarts, m.callKey(ctx))
	m.mu.Unlock()
	return err
}

// MetricsSnapshot is a point-in-time view of the metrics counters.
type MetricsSnapshot struct {
	Started   int64 `json:"started"`
	Succeeded int64 `json:"succeeded"`
	Failed    int64 `json:"failed"`
	InFlight  int64 `json:"in_flight"`
}

// Snapshot returns a point-in-time copy of the in-memory counters.
func (m *MetricsInterceptor) Snapshot() MetricsSnapshot {
	m.mu.Lock()
	defer m.mu.Unlock()
	return MetricsSnapshot{
		Started:   m.started,
		Succeeded: m.succeeded,
		Failed:    m.failed,
		InFlight:  m.started - m.succeeded - m.failed,
	}
}

// ---------------------------------------------------------------------------
// RetryInterceptor
// ---------------------------------------------------------------------------

// ExponentialBackoff holds backoff configuration for retries.
type ExponentialBackoff struct {
	InitialMs  int     // initial delay in ms (default 100)
	MaxMs      int     // maximum delay in ms (default 30000)
	Multiplier float64 // factor per attempt (default 2.0)
	Jitter     float64 // random jitter factor 0.0-1.0 (default 0.1)
}

// DefaultExponentialBackoff returns backoff defaults matching the Python binding.
func DefaultExponentialBackoff() ExponentialBackoff {
	return ExponentialBackoff{
		InitialMs:  100,
		MaxMs:      30_000,
		Multiplier: 2.0,
		Jitter:     0.1,
	}
}

// RetryPolicy describes when and how to retry an RPC call.
type RetryPolicy struct {
	MaxAttempts int                // max attempts including the first (default 3)
	Backoff     ExponentialBackoff // backoff configuration
}

// DefaultRetryPolicy returns a RetryPolicy with sensible defaults.
func DefaultRetryPolicy() RetryPolicy {
	return RetryPolicy{
		MaxAttempts: 3,
		Backoff:     DefaultExponentialBackoff(),
	}
}

// RetryInterceptor provides retry policy hints for client calls.
// It does not perform retries itself -- the client loop reads ShouldRetry
// and BackoffSeconds to decide whether and when to retry.
type RetryInterceptor struct {
	BaseInterceptor
	Policy         RetryPolicy
	RetryableCodes map[StatusCode]bool
}

// NewRetryInterceptor creates a RetryInterceptor with sensible defaults.
func NewRetryInterceptor() *RetryInterceptor {
	return &RetryInterceptor{
		Policy:         DefaultRetryPolicy(),
		RetryableCodes: map[StatusCode]bool{StatusUnavailable: true},
	}
}

// ShouldRetry returns true if the call is idempotent and the error code is
// in the retryable set.
func (r *RetryInterceptor) ShouldRetry(ctx *CallContext, err *RpcError) bool {
	return ctx.Idempotent && r.RetryableCodes[err.Code]
}

// BackoffSeconds returns the delay before the next retry attempt, in seconds.
// The delay grows exponentially with jitter.
func (r *RetryInterceptor) BackoffSeconds(attempt int) float64 {
	b := r.Policy.Backoff
	exp := math.Max(0, float64(attempt-1))
	delayMs := math.Min(float64(b.MaxMs), float64(b.InitialMs)*math.Pow(b.Multiplier, exp))
	jitter := delayMs * b.Jitter * rand.Float64()
	return (delayMs + jitter) / 1000.0
}
