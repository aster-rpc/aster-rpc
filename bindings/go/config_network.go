//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"fmt"
	"strings"
)

// Discovery presets for AsterConfig.Discovery (C ABI aster_discovery_t). Only
// DiscoveryDefault contacts n0 infrastructure (pkarr and DNS at iroh.link);
// relays are configured separately (RelayMode).
const (
	DiscoveryDefault = "default" // n0 pkarr + DNS at iroh.link
	DiscoveryNone    = "none"    // no address lookup: dial peers by full address
	DiscoveryLocal   = "local"   // local-network mDNS only
	DiscoveryCustom  = "custom"  // only DiscoveryDNSOrigin / DiscoveryPkarrRelays
)

// hasNetworkSettings reports whether any C ABI 7.3 network setting differs
// from its default, so toNative only allocates config storage when needed.
func (c *AsterConfig) hasNetworkSettings() bool {
	d := strings.ToLower(strings.TrimSpace(c.Discovery))
	return c.BindAddrV6 != "" || len(c.AdvertiseAddrs) > 0 || len(c.RelayCACerts) > 0 ||
		c.RelayCAOnly || (d != "" && d != DiscoveryDefault) || c.DiscoveryDNSOrigin != "" ||
		len(c.DiscoveryPkarrRelays) > 0
}

// setNetworkNative copies the network settings into n's library storage
// (aster_config_set_network). The caller releases n with aster_config_release.
func (c *AsterConfig) setNetworkNative(a *cArena, n *C.aster_config_t) error {
	if !c.hasNetworkSettings() {
		return nil
	}
	var net C.aster_network_config_t
	net.struct_size = C.uint32_t(C.sizeof_aster_network_config_t)
	switch strings.ToLower(strings.TrimSpace(c.Discovery)) {
	case "", DiscoveryDefault:
		net.discovery = C.ASTER_DISCOVERY_DEFAULT
	case DiscoveryNone:
		net.discovery = C.ASTER_DISCOVERY_NONE
	case DiscoveryLocal:
		net.discovery = C.ASTER_DISCOVERY_LOCAL
	case DiscoveryCustom:
		net.discovery = C.ASTER_DISCOVERY_CUSTOM
	default:
		return fmt.Errorf("Discovery %q: want default, none, local or custom: %w", c.Discovery, ErrInvalidArgument)
	}
	net.bind_addr_v6 = a.str(c.BindAddrV6)
	net.advertise_addrs = a.list(c.AdvertiseAddrs)
	net.relay_ca_certs = a.list(c.RelayCACerts)
	net.relay_ca_only = boolToUint32(c.RelayCAOnly)
	net.discovery_dns_origin = a.str(c.DiscoveryDNSOrigin)
	net.discovery_pkarr_relays = a.list(c.DiscoveryPkarrRelays)
	if err := ffiStatus(func() C.int32_t { return C.aster_config_set_network(n, &net) }); err != nil {
		return fmt.Errorf("aster_config_set_network: %w", err)
	}
	return nil
}

// networkFromNative reads the network settings of n (aster_config_network).
func (c *AsterConfig) networkFromNative(n *C.aster_config_t) {
	var net C.aster_network_config_t
	net.struct_size = C.uint32_t(C.sizeof_aster_network_config_t)
	if C.aster_config_network(n, &net) != C.ASTER_STATUS_OK {
		return
	}
	switch net.discovery {
	case C.ASTER_DISCOVERY_NONE:
		c.Discovery = DiscoveryNone
	case C.ASTER_DISCOVERY_LOCAL:
		c.Discovery = DiscoveryLocal
	case C.ASTER_DISCOVERY_CUSTOM:
		c.Discovery = DiscoveryCustom
	default:
		c.Discovery = DiscoveryDefault
	}
	c.BindAddrV6 = string(viewBytes(net.bind_addr_v6))
	c.AdvertiseAddrs = readList(net.advertise_addrs)
	c.RelayCACerts = readList(net.relay_ca_certs)
	c.RelayCAOnly = net.relay_ca_only != 0
	c.DiscoveryDNSOrigin = string(viewBytes(net.discovery_dns_origin))
	c.DiscoveryPkarrRelays = readList(net.discovery_pkarr_relays)
}

// releaseNative frees the library storage of a config built by toNative.
func releaseNative(n *C.aster_config_t) { C.aster_config_release(n) }
