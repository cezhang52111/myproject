use pallet_contracts::MockExt;
use pallet_contracts::gas::{Gas, GasMeter};
use std::env;
use std::fs;


const CODE_GET_STORAGE: &str = r#"
(module
	(import "env" "ext_get_storage" (func $ext_get_storage (param i32) (result i32)))
	(import "env" "ext_scratch_size" (func $ext_scratch_size (result i32)))
	(import "env" "ext_scratch_read" (func $ext_scratch_read (param i32 i32 i32)))
	(import "env" "ext_return" (func $ext_return (param i32 i32)))
	(import "env" "memory" (memory 1 1))

	(func $assert (param i32)
		(block $ok
			(br_if $ok
				(get_local 0)
			)
			(unreachable)
		)
	)

	(func (export "call")
		(local $buf_size i32)


		;; Load a storage value into the scratch buf.
		(call $assert
			(i32.eq
				(call $ext_get_storage
					(i32.const 4)		;; The pointer to the storage key to fetch
				)

				;; Return value 0 means that the value is found and there were
				;; no errors.
				(i32.const 0)
			)
		)

		;; Find out the size of the scratch buffer
		(set_local $buf_size
			(call $ext_scratch_size)
		)

		;; Copy scratch buffer into this contract memory.
		(call $ext_scratch_read
			(i32.const 36)		;; The pointer where to store the scratch buffer contents,
								;; 36 = 4 + 32
			(i32.const 0)		;; Offset from the start of the scratch buffer.
			(get_local			;; Count of bytes to copy.
				$buf_size
			)
		)

		;; Return the contents of the buffer
		(call $ext_return
			(i32.const 36)
			(get_local $buf_size)
		)

		;; env:ext_return doesn't return, so this is effectively unreachable.
		(unreachable)
	)

	(func (export "deploy"))

	(data (i32.const 4)
		"\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11"
		"\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11"
	)
)
"#;

const CODE_DESTROY_AND_TRANSFER: &str = r#"
(module
	(import "env" "ext_scratch_size" (func $ext_scratch_size (result i32)))
	(import "env" "ext_scratch_read" (func $ext_scratch_read (param i32 i32 i32)))
	(import "env" "ext_get_storage" (func $ext_get_storage (param i32) (result i32)))
	(import "env" "ext_set_storage" (func $ext_set_storage (param i32 i32 i32 i32)))
	(import "env" "ext_call" (func $ext_call (param i32 i32 i64 i32 i32 i32 i32) (result i32)))
	(import "env" "ext_instantiate" (func $ext_instantiate (param i32 i32 i64 i32 i32 i32 i32) (result i32)))
	(import "env" "memory" (memory 1 1))
	(func $assert (param i32)
		(block $ok
			(br_if $ok
				(get_local 0)
			)
			(unreachable)
		)
	)
	(func (export "deploy")
		;; Input data is the code hash of the contract to be deployed.
		(call $assert
			(i32.eq
				(call $ext_scratch_size)
				(i32.const 32)
			)
		)
		;; Copy code hash from scratch buffer into this contract's memory.
		(call $ext_scratch_read
			(i32.const 48)		;; The pointer where to store the scratch buffer contents,
			(i32.const 0)		;; Offset from the start of the scratch buffer.
			(i32.const 32)		;; Count of bytes to copy.
		)
		;; Deploy the contract with the provided code hash.
		(call $assert
			(i32.eq
				(call $ext_instantiate
					(i32.const 48)	;; Pointer to the code hash.
					(i32.const 32)	;; Length of the code hash.
					(i64.const 0)	;; How much gas to devote for the execution. 0 = all.
					(i32.const 0)	;; Pointer to the buffer with value to transfer
					(i32.const 8)	;; Length of the buffer with value to transfer.
					(i32.const 0)	;; Pointer to input data buffer address
					(i32.const 0)	;; Length of input data buffer
				)
				(i32.const 0)
			)
		)
		;; Read the address of the instantiated contract into memory.
		(call $assert
			(i32.eq
				(call $ext_scratch_size)
				(i32.const 8)
			)
		)
		(call $ext_scratch_read
			(i32.const 80)		;; The pointer where to store the scratch buffer contents,
			(i32.const 0)		;; Offset from the start of the scratch buffer.
			(i32.const 8)		;; Count of bytes to copy.
		)
		;; Store the return address.
		(call $ext_set_storage
			(i32.const 16)	;; Pointer to the key
			(i32.const 1)	;; Value is not null
			(i32.const 80)	;; Pointer to the value
			(i32.const 8)	;; Length of the value
		)
	)
	(func (export "call")
		;; Read address of destination contract from storage.
		(call $assert
			(i32.eq
				(call $ext_get_storage
					(i32.const 16)	;; Pointer to the key
				)
				(i32.const 0)
			)
		)
		(call $assert
			(i32.eq
				(call $ext_scratch_size)
				(i32.const 8)
			)
		)
		(call $ext_scratch_read
			(i32.const 80)		;; The pointer where to store the contract address.
			(i32.const 0)		;; Offset from the start of the scratch buffer.
			(i32.const 8)		;; Count of bytes to copy.
		)
		;; Calling the destination contract with non-empty input data should fail.
		(call $assert
			(i32.eq
				(call $ext_call
					(i32.const 80)	;; Pointer to destination address
					(i32.const 8)	;; Length of destination address
					(i64.const 0)	;; How much gas to devote for the execution. 0 = all.
					(i32.const 0)	;; Pointer to the buffer with value to transfer
					(i32.const 8)	;; Length of the buffer with value to transfer
					(i32.const 0)	;; Pointer to input data buffer address
					(i32.const 1)	;; Length of input data buffer
				)
				(i32.const 0x0100)
			)
		)
		;; Call the destination contract regularly, forcing it to self-destruct.
		(call $assert
			(i32.eq
				(call $ext_call
					(i32.const 80)	;; Pointer to destination address
					(i32.const 8)	;; Length of destination address
					(i64.const 0)	;; How much gas to devote for the execution. 0 = all.
					(i32.const 8)	;; Pointer to the buffer with value to transfer
					(i32.const 8)	;; Length of the buffer with value to transfer
					(i32.const 0)	;; Pointer to input data buffer address
					(i32.const 0)	;; Length of input data buffer
				)
				(i32.const 0)
			)
		)
		;; Calling the destination address with non-empty input data should now work since the
		;; contract has been removed. Also transfer a balance to the address so we can ensure this
		;; does not keep the contract alive.
		(call $assert
			(i32.eq
				(call $ext_call
					(i32.const 80)	;; Pointer to destination address
					(i32.const 8)	;; Length of destination address
					(i64.const 0)	;; How much gas to devote for the execution. 0 = all.
					(i32.const 0)	;; Pointer to the buffer with value to transfer
					(i32.const 8)	;; Length of the buffer with value to transfer
					(i32.const 0)	;; Pointer to input data buffer address
					(i32.const 1)	;; Length of input data buffer
				)
				(i32.const 0)
			)
		)
	)
	(data (i32.const 0) "\00\00\01")		;; Endowment to send when creating contract.
	(data (i32.const 8) "")		;; Value to send when calling contract.
	(data (i32.const 16) "")	;; The key to store the contract address under.
)
"#;

const CODE_SET_RENT: &str = r#"
(module
	(import "env" "ext_dispatch_call" (func $ext_dispatch_call (param i32 i32)))
	(import "env" "ext_set_storage" (func $ext_set_storage (param i32 i32 i32 i32)))
	(import "env" "ext_set_rent_allowance" (func $ext_set_rent_allowance (param i32 i32)))
	(import "env" "ext_scratch_size" (func $ext_scratch_size (result i32)))
	(import "env" "ext_scratch_read" (func $ext_scratch_read (param i32 i32 i32)))
	(import "env" "memory" (memory 1 1))
	;; insert a value of 4 bytes into storage
	(func $call_0
		(call $ext_set_storage
			(i32.const 1)
			(i32.const 1)
			(i32.const 0)
			(i32.const 4)
		)
	)
	;; remove the value inserted by call_1
	(func $call_1
		(call $ext_set_storage
			(i32.const 1)
			(i32.const 0)
			(i32.const 0)
			(i32.const 0)
		)
	)
	;; transfer 50 to ALICE
	(func $call_2
		(call $ext_dispatch_call
			(i32.const 68)
			(i32.const 11)
		)
	)
	;; do nothing
	(func $call_else)
	(func $assert (param i32)
		(block $ok
			(br_if $ok
				(get_local 0)
			)
			(unreachable)
		)
	)
	;; Dispatch the call according to input size
	(func (export "call")
		(local $input_size i32)
		(set_local $input_size
			(call $ext_scratch_size)
		)
		(block $IF_ELSE
			(block $IF_2
				(block $IF_1
					(block $IF_0
						(br_table $IF_0 $IF_1 $IF_2 $IF_ELSE
							(get_local $input_size)
						)
						(unreachable)
					)
					(call $call_0)
					return
				)
				(call $call_1)
				return
			)
			(call $call_2)
			return
		)
		(call $call_else)
	)
	;; Set into storage a 4 bytes value
	;; Set call set_rent_allowance with input
	(func (export "deploy")
		(local $input_size i32)
		(set_local $input_size
			(call $ext_scratch_size)
		)
		(call $ext_set_storage
			(i32.const 0)
			(i32.const 1)
			(i32.const 0)
			(i32.const 4)
		)
		(call $ext_scratch_read
			(i32.const 0)
			(i32.const 0)
			(get_local $input_size)
		)
		(call $ext_set_rent_allowance
			(i32.const 0)
			(get_local $input_size)
		)
	)
	;; Encoding of 10 in balance
	(data (i32.const 0) "\28")
	;; Encoding of call transfer 50 to CHARLIE
	(data (i32.const 68) "\00\00\03\00\00\00\00\00\00\00\C8")
)
"#;

fn main() {
    println!("Hello, world!");
    let mut mock_ext = MockExt::default();
    // mock_ext
	// 		.storage
    // 		.insert([0x11; 32], [0x22; 32].to_vec());
    let contents = fs::read_to_string("flipper.wat").expect("Something wrong");
    let output = pallet_contracts::execute(
        &contents,
        vec![0x8C,0x97,0xDB,0x39],
        &mut mock_ext,
        &mut GasMeter::with_limit(100_000, 1),
    );

    let output = pallet_contracts::execute(
        &contents,
        vec![0x25, 0x44, 0x4A, 0xFE],
        &mut mock_ext,
        &mut GasMeter::with_limit(100_000, 1),
    );
    println!("get storage puts data into scratch" );
    let wkv = mock_ext.write_key_value;
    for (k, v) in &wkv {
        println!("write k: {:?}, v: {:?}", k, v);
    }
    
    let rkv = mock_ext.read_key_value;
    for (k, v) in &rkv {
        println!("read k: {:?}, v: {:?}", k, v);
    }


}
