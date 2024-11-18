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

**Request the recommended priority fee details**
The purpose of this API is to understand what data is taken into consideration given the query.
The response shows the statistical distribution of data per account as well as how much data is available in each account.
The request is identical to the one used in getPriorityFeeEstimate request. This is to ensure that same request could be
reused during analysis and during dev / operational stages

```json
{
  "id": "version1",
  "jsonrpc": "2.0",
  "method": "getPriorityFeeEstimateDetails",
  "params": [
    {
      "accountKeys": [
"JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"
      ],
      "options": {"recommended": true}
    }
  ]
}
```

**Response**

```json
{
  "jsonrpc": "2.0",
  "result": {
    "priorityFeeEstimateDetails": [
      [
        "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",
        {
          "estimates": {
            "min": 0.0,
            "low": 0.0,
            "medium": 0.0,
            "high": 0.0,
            "veryHigh": 0.0,
            "unsafeMax": 25113388.0
          },
          "mean": 717525.0, // mean fee payed for each transaction for account evaluated over last 150 (or less if requested less) slots 
          "stdev": 4244937.0, // standard deviation of fee payed for each transaction for account evaluated over last 150 (or less if requested less) slots ,
          "skew": null, // skew of fee payed for each transaction for account evaluated over last 150 (or less if requested less) slots. Null if data is randomly distributed and cannot calculate
          "count": 35  // Number of transactions for account that were evaluated over last 150 (or less if requested less) slots
        }
      ],
      [
        "Global",
        {
          "estimates": {
            "min": 0.0,
            "low": 0.0,
            "medium": 20003.0,
            "high": 2276532.0,
            "veryHigh": 32352142.0,
            "unsafeMax": 2000000000.0
          },
          "mean": 8118956.0, // mean fee payed for each transaction for all accounts evaluated over last 150 (or less if requested less) slots
          "stdev": 53346050.0, // standard deviation of fee payed for each transaction for all account evaluated over last 150 (or less if requested less) slots ,
          "skew": null, // skew of fee payed for each transaction for all accounts evaluated over last 150 (or less if requested less) slots. Null if data is randomly distributed and cannot calculate
          "count": 14877  // Number of transactions for all accounts that were evaluated over last 150 (or less if requested less) slots
        }
      ]
    ],
    "priorityFeeEstimate": 20003.0
  },
  "id": "version1"
}
```