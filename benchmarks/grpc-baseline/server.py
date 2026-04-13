"""gRPC Mission Control server -- baseline benchmark.

Replicates the Aster Mission Control getStatus handler with identical
logic so throughput comparisons are apples-to-apples.

Usage:
    cd benchmarks/grpc-baseline
    python -m grpc_tools.protoc -I. --python_out=. --grpc_python_out=. mission_control.proto
    python server.py
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
    server = grpc.aio.server()
    pb2_grpc.add_MissionControlServicer_to_server(
        MissionControlServicer(), server
    )
    server.add_insecure_port("127.0.0.1:50055")
    await server.start()
    print("gRPC server listening on [::]:50051")
    await server.wait_for_termination()


if __name__ == "__main__":
    asyncio.run(serve())
