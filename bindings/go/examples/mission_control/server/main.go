//go:build cgo

// Mission Control server.
//
//	go run ./examples/mission_control/server            # open
//	go run ./examples/mission_control/server -auth      # the auth chapter
//
// Everything comes from the configuration: ASTER_* variables
// (aster.ConfigFromEnv), or an aster.toml with -config. With -auth the
// server needs enrollment configured (ASTER_ROOT_PUBKEY,
// ASTER_ENROLLMENT_AUDIENCE, ASTER_ENROLLMENT_FLEET_NAMESPACE,
// ASTER_ENROLLMENT_CREDENTIAL, ASTER_ENROLLMENT_STATE_PATH; see the README):
// it admits only nodes holding a credential from that root, and each
// method's Requires is checked against the roles in the caller's credential.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log"
	"os"
	"os/signal"

	aster "sdk.getaster.now/go"
	"sdk.getaster.now/go/examples/mission_control/mc"
)

func main() {
	auth := flag.Bool("auth", false, "serve the auth chapter (requires enrollment configured)")
	configFile := flag.String("config", "", "aster.toml (default: ASTER_* environment variables)")
	flag.Parse()
	if err := run(*auth, *configFile); err != nil {
		log.Fatal(err)
	}
}

func run(auth bool, configFile string) error {
	cfg, err := aster.ConfigFromEnv()
	if configFile != "" {
		cfg, err = aster.ConfigFromTOML(configFile)
	}
	if err != nil {
		return err
	}
	if auth && !cfg.IsEnrollmentEnabled() {
		return errors.New("-auth needs enrollment configured: ASTER_ROOT_PUBKEY, ASTER_ENROLLMENT_AUDIENCE, " +
			"ASTER_ENROLLMENT_FLEET_NAMESPACE, ASTER_ENROLLMENT_CREDENTIAL and ASTER_ENROLLMENT_STATE_PATH (see the README)")
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	srv, err := aster.StartServer(ctx, aster.ServerOptions{Config: cfg, Services: []*aster.Service{mc.Service(auth)}})
	if err != nil {
		return err
	}
	defer srv.Close()
	id, _ := srv.NodeID()
	ticket, err := srv.Ticket(ctx)
	if err != nil {
		return err
	}
	fmt.Println("Iroh node id:", id)
	fmt.Println("Address:", ticket)
	<-ctx.Done()
	return nil
}
