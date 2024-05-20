### Priority Fee Estimator

This repo contains the RPC service that supports the `getPriorityFeeEstimate` method. This method returns the estimated priority fee for a transaction to be included in the next block. The service uses historical priority fee data to estimate the fee.

See a further description of functionality [here](https://docs.helius.dev/solana-rpc-nodes/alpha-priority-fee-api)

#### Running the service

The service requires the following envs

`RPC_URL` - RPC url of a Solana node
`GRPC_URL` - Yellowstone gRPC url
`GRPC_X_TOKEN` - Yellowstone gRPC token (some endpoints may not require this)

To run the service run

```bash
cargo run
```

#### Example queries

**Request all priority fee levels for Jup v6**

```json
{
  "jsonrpc": "2.0",
  "id": "1",
  "method": "getPriorityFeeEstimate",
  "params": [
    {
      "accountKeys": ["JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
      "options": {
        "includeAllPriorityFeeLevels": true
      }
    }
  ]
}
```

**Response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "priorityFeeLevels": {
      "min": 0.0,
      "low": 2.0,
      "medium": 10082.0,
      "high": 100000.0,
      "veryHigh": 1000000.0,
      "unsafeMax": 50000000.0
    }
  },
  "id": "1"
}
```

**Request the recommended priority level for Jup v6**

```json
{
  "jsonrpc": "2.0",
  "id": "1",
  "method": "getPriorityFeeEstimate",
  "params": [
    {
      "accountKeys": ["JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
      "options": {
        "recommended": true
      }
    }
  ]
}
```

**Response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "priorityFeeEstimate": 71428.0
  },
  "id": "1"
}
```

**Request the recommended priority fee excluding votes**

```json
{
  "jsonrpc": "2.0",
  "id": "1",
  "method": "getPriorityFeeEstimate",
  "params": [
    {
      "accountKeys": ["JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
      "options": {
        "includeVote": false
      }
    }
  ]
}
```

**Response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "priorityFeeEstimate": 71428.0
  },
  "id": "1"
}
```
