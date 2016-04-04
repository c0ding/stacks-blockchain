#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""
    Blockstack
    ~~~~~
    copyright: (c) 2014-2015 by Halfmoon Labs, Inc.
    copyright: (c) 2016 by Blockstack.org

    This file is part of Blockstack

    Blockstack is free software: you can redistribute it and/or modify
    it under the terms of the GNU General Public License as published by
    the Free Software Foundation, either version 3 of the License, or
    (at your option) any later version.

    Blockstack is distributed in the hope that it will be useful,
    but WITHOUT ANY WARRANTY; without even the implied warranty of
    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    GNU General Public License for more details.
    You should have received a copy of the GNU General Public License
    along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
"""

from pybitcoin import embed_data_in_blockchain, make_op_return_tx, make_op_return_outputs, \
        make_op_return_script, broadcast_transaction, serialize_transaction, \
        script_hex_to_address, get_unspents
from utilitybelt import is_hex
from binascii import hexlify, unhexlify

from ..b40 import b40_to_hex, bin_to_b40, is_b40
from ..config import *
from ..scripts import *

from ..nameset import *

# consensus hash fields (ORDER MATTERS!)
FIELDS = NAMEREC_FIELDS[:]

# fields that this operation changes
MUTATE_FIELDS = NAMEREC_MUTATE_FIELDS[:] + [
    'revoked',
    'value_hash',
    'sender_pubkey'
]

# fields to back up when applying this operation 
BACKUP_FIELDS = NAMEREC_BACKUP_FIELDS[:] + MUTATE_FIELDS[:] + [
    'consensus_hash'
]

def build(name, testset=False):
    """
    Takes in the name, including the namespace ID (but not the id: scheme)
    Returns a hex string representing up to LENGTHS['blockchain_id_name'] bytes.
    
    Record format:
    
    0    2  3                             39
    |----|--|-----------------------------|
    magic op   name.ns_id (37 bytes)
    
    """
    
    if not is_name_valid( name ):
       raise Exception("Invalid name '%s'" % name)

    readable_script = "NAME_REVOKE 0x%s" % (hexlify(name))
    hex_script = blockstack_script_to_hex(readable_script)
    packaged_script = add_magic_bytes(hex_script, testset=testset)
    
    return packaged_script 


@state_transition("name", "name_records")
def check( state_engine, nameop, block_id, checked_ops ):
    """
    Revoke a name--make it available for registration.
    * it must be well-formed
    * its namespace must be ready.
    * the name must be registered
    * it must be sent by the name owner

    NAME_REVOKE isn't allowed during an import, so the name's namespace must be ready.

    Return True if accepted
    Return False if not
    """

    name = nameop['name']
    sender = nameop['sender']
    namespace_id = get_namespace_from_name( name )

    # name must be well-formed
    if not is_b40( name ) or "+" in name or name.count(".") > 1:
        log.debug("Malformed name '%s': non-base-38 characters" % name)
        return False

    # name must exist
    name_rec = state_engine.get_name( name )
    if name_rec is None:
        log.debug("Name '%s' does not exist" % name)
        return False

    # namespace must be ready
    if not state_engine.is_namespace_ready( namespace_id ):
       log.debug("Namespace '%s' is not ready" % namespace_id )
       return False

    # name must not be revoked
    if state_engine.is_name_revoked( name ):
        log.debug("Name '%s' is revoked" % name)
        return False

    # name must not be expired
    if state_engine.is_name_expired( name, block_id ):
        log.debug("Name '%s' is expired" % name)
        return False

    # the name must be registered
    if not state_engine.is_name_registered( name ):
       log.debug("Name '%s' is not registered" % name )
       return False

    # the sender must own this name
    if not state_engine.is_name_owner( name, sender ):
       log.debug("Name '%s' is not owned by %s" % (name, sender))
       return False

    # apply state transition 
    nameop['revoked'] = True
    nameop['value_hash'] = None
    return True


def tx_extract( payload, senders, inputs, outputs, block_id, vtxindex, txid ):
    """
    Extract and return a dict of fields from the underlying blockchain transaction data
    that are useful to this operation.

    Required (+ parse):
    sender:  the script_pubkey (as a hex string) of the principal that sent the name preorder transaction
    address:  the address from the sender script

    Optional:
    sender_pubkey_hex: the public key of the sender
    """
  
    sender_script = None 
    sender_address = None 
    sender_pubkey_hex = None

    try:

       # by construction, the first input comes from the principal
       # who sent the registration transaction...
       assert len(senders) > 0
       assert 'script_pubkey' in senders[0].keys()
       assert 'addresses' in senders[0].keys()

       sender_script = str(senders[0]['script_pubkey'])
       sender_address = str(senders[0]['addresses'][0])

       assert sender_script is not None 
       assert sender_address is not None

       if str(senders[0]['script_type']) == 'pubkeyhash':
          sender_pubkey_hex = get_public_key_hex_from_tx( inputs, sender_address )

    except Exception, e:
       log.exception(e)
       raise Exception("Failed to extract")

    parsed_payload = parse( payload )
    assert parsed_payload is not None 

    ret = {
       "sender": sender_script,
       "address": sender_address,
       "txid": txid,
       "vtxindex": vtxindex,
       "op": NAME_REVOKE
    }

    ret.update( parsed_payload )

    if sender_pubkey_hex is not None:
        ret['sender_pubkey'] = sender_pubkey_hex

    return ret


def make_outputs( data, inputs, change_address, pay_fee=True ):
    """
    Make outputs for a revoke.
    """

    outputs = [
        # main output
        {"script_hex": make_op_return_script(data, format='hex'),
         "value": 0},
        
        # change output
        {"script_hex": make_pay_to_address_script(change_address),
         "value": calculate_change_amount(inputs, 0, 0)}
    ]

    if pay_fee:
        dust_fee = tx_dust_fee_from_inputs_and_outputs( inputs, outputs )
        outputs[1]['value'] = calculate_change_amount( inputs, 0, dust_fee )

    return outputs


def broadcast(name, private_key, blockchain_client, testset=False, blockchain_broadcaster=None, user_public_key=None, tx_only=False):
    
    # sanity check 
    pay_fee = True
    if user_public_key is not None:
        pay_fee = False
        tx_only = True

    if user_public_key is None and private_key is None:
        raise Exception("Missing both public and private key")
    
    if not tx_only and private_key is None:
        raise Exception("Need private key for broadcasting")
    
    if blockchain_broadcaster is None:
        blockchain_broadcaster = blockchain_client 
    
    from_address = None 
    inputs = None
    private_key_obj = None
    
    if user_public_key is not None:
        # subsidizing 
        pubk = BitcoinPublicKey( user_public_key )

        from_address = pubk.address()
        inputs = get_unspents( from_address, blockchain_client )

    elif private_key is not None:
        # ordering directly 
        pubk = BitcoinPrivateKey( private_key ).public_key()
        public_key = pubk.to_hex()
        
        private_key_obj, from_address, inputs = analyze_private_key(private_key, blockchain_client)
         
    nulldata = build(name, testset=testset)
    outputs = make_outputs( nulldata, inputs, from_address, pay_fee=pay_fee )
   
    if tx_only:
       
        unsigned_tx = serialize_transaction( inputs, outputs )
        return {'unsigned_tx': unsigned_tx}

    else:
       
        signed_tx = tx_serialize_and_sign( inputs, outputs, private_key_obj )
        response = broadcast_transaction( signed_tx, blockchain_broadcaster )
        response.update({'data': nulldata})
        return response


def parse(bin_payload):    
    """
    Interpret a block's nulldata back into a name.  The first three bytes (2 magic + 1 opcode)
    will not be present in bin_payload.
    
    The name will be directly represented by the bytes given.
    """
    
    fqn = bin_payload
    if not is_name_valid( fqn ):
        return None 

    return {
       'opcode': 'NAME_REVOKE',
       'name': fqn
    }


def get_fees( inputs, outputs ):
    """
    Given a transaction's outputs, look up its fees:
    * there should be two outputs: the OP_RETURN and change address
    
    Return (dust fees, operation fees) on success 
    Return (None, None) on invalid output listing
    """
    if len(outputs) != 2:
        return (None, None)
    
    # 0: op_return
    if not tx_output_is_op_return( outputs[0] ):
        return (None, None) 
    
    if outputs[0]["value"] != 0:
        return (None, None) 
    
    # 1: change address 
    if script_hex_to_address( outputs[1]["script_hex"] ) is None:
        return (None, None)
    
    dust_fee = (len(inputs) + 1) * DEFAULT_DUST_FEE + DEFAULT_OP_RETURN_FEE
    op_fee = 0
    
    return (dust_fee, op_fee)


def restore_delta( name_rec, block_number, history_index, untrusted_db, testset=False ):
    """
    Find the fields in a name record that were changed by an instance of this operation, at the 
    given (block_number, history_index) point in time in the past.  The history_index is the
    index into the list of changes for this name record in the given block.

    Return the fields that were modified on success.
    Return None on error.
    """
    
    from ..nameset import BlockstackDB

    name_rec_script = build( str(name_rec['name']), testset=testset )
    name_rec_payload = unhexlify( name_rec_script )[3:]
    ret_op = parse( name_rec_payload )

    return ret_op


def snv_consensus_extras( name_rec, block_id, blockchain_name_data, db ):
    """
    Calculate any derived missing data that goes into the check() operation,
    given the block number, the name record at the block number, and the db.
    """

    ret_op = {}
    return ret_op

