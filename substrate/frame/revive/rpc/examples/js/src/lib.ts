import {
	Contract,
	ContractFactory,
	JsonRpcProvider,
	TransactionReceipt,
	TransactionResponse,
	Wallet,
} from 'ethers'
import { readFileSync } from 'node:fs'
import type { compile } from '@parity/revive'
import { spawn } from 'node:child_process'
import { parseArgs } from 'node:util'

type CompileOutput = Awaited<ReturnType<typeof compile>>
type Abi = CompileOutput['contracts'][string][string]['abi']

const {
	values: { geth, westend, ['private-key']: privateKey },
} = parseArgs({
	args: process.argv.slice(2),
	options: {
		['private-key']: {
			type: 'string',
			short: 'k',
		},
		geth: {
			type: 'boolean',
		},
		westend: {
			type: 'boolean',
		},
	},
})

if (geth) {
	console.log('Testing with Geth')
	const child = spawn(
		'geth',
		[
			'--http',
			'--http.api',
			'web3,eth,debug,personal,net',
			'--http.port',
			'8545',
			'--dev',
			'--verbosity',
			'0',
		],
		{ stdio: 'inherit' }
	)

	process.on('exit', () => child.kill())
	child.unref()
	await new Promise((resolve) => setTimeout(resolve, 500))
}

const provider = new JsonRpcProvider(
	westend ? 'https://westend-asset-hub-eth-rpc.polkadot.io' : 'http://localhost:8545'
)

const signer = privateKey ? new Wallet(privateKey, provider) : await provider.getSigner()
console.log(`Signer address: ${await signer.getAddress()}, Nonce: ${await signer.getNonce()}`)

/**
 * Get one of the pre-built contracts
 * @param name - the contract name
 */
export function getContract(name: string): { abi: Abi; bytecode: string } {
	const file = geth
		? readFileSync('evm-contracts.json', 'utf8')
		: readFileSync('pvm-contracts.json', 'utf8')
	const contracts = JSON.parse(file) as Record<string, { abi: Abi; bytecode: string }>
	return contracts[name]
}

/**
 * Deploy a contract
 * @returns the contract address
 **/
export async function deploy(bytecode: string, abi: Abi, args: any[] = []): Promise<string> {
	console.log('Deploying contract with', args)
	const contractFactory = new ContractFactory(abi, bytecode, signer)

	const contract = await contractFactory.deploy(args)
	await contract.waitForDeployment()
	const address = await contract.getAddress()
	console.log(`Contract deployed: ${address}`)
	return address
}

/**
 * Call a contract
 **/
export async function call(
	method: string,
	address: string,
	abi: Abi,
	args: any[] = [],
	opts: { value?: bigint } = {}
): Promise<null | TransactionReceipt> {
	console.log(`Calling ${method} at ${address} with`, args, opts)
	const contract = new Contract(address, abi, signer)
	const tx = (await contract[method](...args, opts)) as TransactionResponse
	console.log('Call transaction hash:', tx.hash)
	return tx.wait()
}
