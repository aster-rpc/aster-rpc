
# Pre Requisite (user will do this)

## 1. Generate offline root key (one-time)
aster keygen root → root.key
root.key should be stored securely using keyring or failing that in a private user file

# Producer Setup (user will do this)

## 1. Generate stable producer key
aster keygen producer → node.key           # prints NodeId

## 2. Sign an enrollment credential for this producer node
aster authorize --root-key ./root.key \
                --producer-id <NodeId> → enrollment.token

note: root key is optional, if there's one that's stored securely we can use it

# Producer code

Create an example service in examples/python/simple_producer.py
1. It will take node key and producer token as environment variables or generate random ones and print base64 strings to console (this might be built-in to the RPC code)
2. Print the Endpoint ticket
3. Print the contract-id
3. Should be a simple hello world

# Path A: Generate Python typed client

## On producer machine:

aster generate client --lang python --contract-id XXX → client.zip


Transfer to the client machine:
`(EndpointTicket, consumer_enrollment.token, client.zip)`.

## On client machine:
conceptually...
```
# .env
ASTER_PEER_TICKET=<EndpointTicket>
ASTER_ENROLLMENT=<consumer_enrollment.token>
ASTER_ACCESS_TOKEN=<consumer.rcan>
```

```python
from my_service_client import MyServiceClient

client = await MyServiceClient.connect()  # reads env vars
result = await client.my_method(...)
```


# Path B: Generate a dynamic client

conceptually...
```python
client = await AsterDynamicClient.connect(
    ticket=os.environ["ASTER_PEER_TICKET"],
    enrollment=os.environ["ASTER_ENROLLMENT"],
    token=os.environ["ASTER_ACCESS_TOKEN"],
)
# admission + contract fetch happen during connect()

result = await client.call("MyService", "MyMethod", {"field": "value"})
```
