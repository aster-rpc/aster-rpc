#!/usr/bin/env python3
"""gRPC benchmark server with TLS -- runs on remote machine.

Usage:
    cd ~/bench && source .venv/bin/activate
    python gen_certs.py  # first time
    python grpc_server_tls.py
"""
import asyncio

import grpc

import mission_control_pb2 as pb2
import mission_control_pb2_grpc as pb2_grpc


class MissionControlServicer(pb2_grpc.MissionControlServicer):
    async def GetStatus(self, request, context):
        return pb2.StatusResponse(
            agent_id=request.agent_id,
            status="running",
            uptime_secs=3600,
        )

    async def SubmitLog(self, request, context):
        return pb2.SubmitLogResult(accepted=True)


async def serve():
    with open("certs/server.key", "rb") as f:
        server_key = f.read()
    with open("certs/server.crt", "rb") as f:
        server_cert = f.read()

    creds = grpc.ssl_server_credentials([(server_key, server_cert)])

    server = grpc.aio.server()
    pb2_grpc.add_MissionControlServicer_to_server(
        MissionControlServicer(), server
    )
    server.add_secure_port("0.0.0.0:50055", creds)
    await server.start()
    print("gRPC TLS server listening on 0.0.0.0:50055", flush=True)
    await server.wait_for_termination()


if __name__ == "__main__":
    asyncio.run(serve())
