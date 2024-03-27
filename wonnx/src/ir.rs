//! DAG representation of ONNX ops allowing for transformations and optimizations before compilation
use crate::onnx::{ModelProto, NodeProto, TensorProto, ValueInfoProto};
use crate::utils::{DataTypeError, Shape};
use std::borrow::Cow;
use std::fmt::Debug;
use std::hash::Hash;
use std::ptr;
use std::{collections::HashMap, sync::Arc};
use thiserror::Error;

#[derive(Clone)]
pub struct OperatorDefinition<'model> {
    pub(crate) proto: Cow<'model, NodeProto>,
    pub(crate) output_shapes: Vec<Shape>,
}

impl<'model> OperatorDefinition<'model> {
    pub fn from(
        node: Cow<'model, NodeProto>,
        value_shapes: &HashMap<&'model str, Shape>,
    ) -> Result<OperatorDefinition<'model>, IrError> {
        let mut output_shapes: Vec<Shape> = Vec::with_capacity(node.get_output().len());
        for output_name in node.get_output() {
            if !value_shapes.contains_key(output_name.as_str()) {
                return Err(IrError::OutputNodeNotFound(output_name.to_string()));
            }

            output_shapes.push(value_shapes[&output_name.as_str()].clone());
        }
        Ok(OperatorDefinition {
            proto: node,
            output_shapes,
        })
    }
}

#[derive(Clone)]
pub enum NodeDefinition<'model> {
    Operator(Box<OperatorDefinition<'model>>),
    Tensor(Box<Cow<'model, TensorProto>>),
    Input(&'model ValueInfoProto),
    Outputs { names: Vec<String> },
    Missing, // A missing input (optional)
}

static MISSING_OPTIONAL_INPUT: NodeDefinition<'static> = NodeDefinition::Missing;

#[derive(Clone)]
pub struct Input<'model> {
    pub source_node: Arc<Node<'model>>,
    pub output_index: usize,
}

pub struct Node<'model> {
    pub definition: NodeDefinition<'model>,
    pub inputs: Vec<Input<'model>>,
}

#[derive(Debug, Error)]
pub enum IrError {
    #[error("output node for output {0} not found")]
    OutputNodeNotFound(String),

    #[error("could not find node corresponding to input {input_name} of node {target_node_name}")]
    InputNodeNotFound {
        target_node_name: String,
        input_name: String,
    },

    #[error("issue with data types: {0}")]
    Type(#[from] DataTypeError),
}

impl<'m> NodeDefinition<'m> {
    pub fn get_name(&self) -> Cow<'_, str> {
        match self {
            // Nodes are identified by their first output's name, because node names are optional (and "only to be used
            // for diagnostic purposes" according to the ONNX IR specification) whereas output names are required and should be unique.
            NodeDefinition::Operator(op_def) => Cow::from(&op_def.proto.get_output()[0]),
            NodeDefinition::Tensor(t) => Cow::from(t.get_name()),
            NodeDefinition::Input(i) => Cow::from(i.get_name()),
            NodeDefinition::Outputs { .. } => Cow::from(" "),
            NodeDefinition::Missing => Cow::from(""),
        }
    }
}

impl NodeProto {
    // Nodes are identified by their first output's name, because node names are optional (and "only to be used
    // for diagnostic purposes" according to the ONNX IR specification) whereas output names are required and should be unique.
    fn unique_name(&self) -> String {
        self.get_output()[0].clone()
    }
}

impl<'model> Node<'model> {
    pub fn new(variant: NodeDefinition<'model>) -> Node<'model> {
        Node {
            definition: variant,
            inputs: vec![],
        }
    }

    pub fn is_dynamic(&self) -> bool {
        matches!(
            self.definition,
            NodeDefinition::Operator(..) | NodeDefinition::Input(..)
        )
    }

    pub fn is_constant(&self) -> bool {
        !self.is_dynamic()
            || (matches!(self.definition, NodeDefinition::Operator(..))
                && self.inputs.iter().all(|i| i.source_node.is_constant()))
    }

    pub fn definition(&self) -> &NodeDefinition<'model> {
        &self.definition
    }

    /// Construct part of the intermediate representation tree for the indicated node.
    pub fn from_node<'a>(
        node: Cow<'model, NodeProto>,
        value_shapes: &HashMap<&'model str, Shape>,
        node_definitions_by_output: &'a HashMap<String, NodeDefinition<'model>>,
        nodes_by_unique_name: &mut HashMap<String, Arc<Node<'model>>>,
    ) -> Result<Arc<Node<'model>>, IrError> {
        let node_name = node.unique_name();
        // Did we already translate this node before?
        if nodes_by_unique_name.contains_key(&node_name) {
            let n = nodes_by_unique_name.get(&node_name).unwrap();
            return Ok(n.clone());
        }

        let inputs: Result<Vec<Input<'model>>, IrError> = node
            .get_input()
            .iter()
            .map(|input_name: &String| {
                let my_input_name = input_name.clone();
                let source_node_definition = node_definitions_by_output
                    .get(&my_input_name)
                    .unwrap_or(&MISSING_OPTIONAL_INPUT);

                Ok(match source_node_definition {
                    // The source is another op - continue translating that node
                    NodeDefinition::Operator(source_node_proto) => Input {
                        source_node: Node::from_node(
                            source_node_proto.proto.clone(),
                            value_shapes,
                            node_definitions_by_output,
                            nodes_by_unique_name,
                        )?,
                        output_index: source_node_proto
                            .proto
                            .get_output()
                            .iter()
                            .position(|s| s == input_name)
                            .ok_or_else(|| {
                                IrError::OutputNodeNotFound(format!(
                                    "from_node {}",
                                    input_name.to_string()
                                ))
                            })?,
                    },
                    _ => {
                        // The source is an initializer or model onput
                        let source_name = source_node_definition.get_name().to_string();

                        Input {
                            output_index: 0,
                            // Did we already translate this node?
                            source_node: match nodes_by_unique_name.get(&source_name) {
                                Some(node) => node.clone(),
                                None => {
                                    let node = Arc::new(Node::new(source_node_definition.clone()));
                                    nodes_by_unique_name.insert(source_name, node.clone());
                                    node
                                }
                            },
                        }
                    }
                })
            })
            .collect();

        let translated = Arc::new(Node {
            definition: NodeDefinition::Operator(Box::new(OperatorDefinition::from(
                node.clone(),
                value_shapes,
            )?)),
            inputs: inputs?,
        });
        nodes_by_unique_name.insert(node.unique_name(), translated.clone());
        Ok(translated)
    }

    /// Construct an intermediate representation graph for calculating the output with the specified name.
    pub fn from_model(
        model: &'model ModelProto,
        outputs: Option<&[String]>,
    ) -> Result<Arc<Node<'model>>, IrError> {
        // Collect value shapes
        let mut value_shapes: HashMap<&'model str, Shape> = HashMap::new();
        for vi in model.get_graph().get_value_info() {
            value_shapes.insert(vi.get_name(), vi.get_shape()?);
        }

        for vi in model.get_graph().get_output() {
            let output_name = vi.get_name();
            if !output_name.is_empty() {
                value_shapes.insert(output_name, vi.get_shape()?);
            }
        }

        // Sort nodes by output nodes
        let mut node_definitions_by_output = HashMap::<String, NodeDefinition<'model>>::new();
        for node in model.get_graph().get_node().iter() {
            let node_def = NodeDefinition::Operator(Box::new(OperatorDefinition::from(
                Cow::Borrowed(node),
                &value_shapes,
            )?));
            for output in node.get_output() {
                if !output.is_empty() {
                    node_definitions_by_output.insert(output.to_string(), node_def.clone());
                }
            }
        }

        // Collect intializer info
        for initializer in model.get_graph().get_initializer().iter() {
            node_definitions_by_output.insert(
                initializer.get_name().to_string(),
                NodeDefinition::Tensor(Box::new(Cow::Borrowed(initializer))),
            );
        }

        let output_names: Vec<String> = match outputs {
            Some(outputs) => outputs.to_vec(),
            None => model
                .get_graph()
                .get_output()
                .iter()
                .map(|x| x.get_name().to_string())
                .collect(),
        };

        // Collect input name
        for input in model.get_graph().get_input().iter() {
            if !node_definitions_by_output.contains_key(input.get_name()) {
                node_definitions_by_output
                    .insert(input.get_name().to_string(), NodeDefinition::Input(input));
            } else {
                log::debug!(
                    "Skipping input definition {}: already defined",
                    input.get_name()
                );
            }
        }

        let mut nodes_by_name = HashMap::new();

        let output_nodes: Result<Vec<Input<'model>>, IrError> = output_names
            .iter()
            .map(|output_name| {
                let output_node = model
                    .get_graph()
                    .get_node()
                    .iter()
                    .find(|x| -> bool { x.get_output().contains(output_name) })
                    .ok_or_else(|| IrError::OutputNodeNotFound(output_name.clone()))?;

                let source_node = Node::<'model>::from_node(
                    Cow::Borrowed(output_node),
                    &value_shapes,
                    &node_definitions_by_output,
                    &mut nodes_by_name,
                )?;

                let output_index = output_node
                    .get_output()
                    .iter()
                    .position(|s| s == output_name)
                    .ok_or_else(|| IrError::OutputNodeNotFound(output_name.clone()))?;

                Ok(Input {
                    source_node,
                    output_index,
                })
            })
            .collect();

        Ok(Arc::new(Node {
            definition: NodeDefinition::Outputs {
                names: output_names,
            },
            inputs: output_nodes?,
        }))
    }
}

impl<'model> Debug for NodeDefinition<'model> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeDefinition::Operator(def) => {
                write!(
                    f,
                    "op: {} ({})",
                    def.proto.get_name(),
                    def.proto.get_op_type()
                )
            }
            NodeDefinition::Tensor(def) => write!(f, "tensor {}", def.get_name()),
            NodeDefinition::Input(def) => write!(f, "input {}", def.get_name()),
            NodeDefinition::Outputs { .. } => write!(f, "outputs"),
            NodeDefinition::Missing => write!(f, "missing (optional)"),
        }
    }
}

/// Wrap an Arc<Node> in a struct so we can implement pointer-based comparison for it, and use them as keys in a HashSet/HashMap
#[derive(Clone)]
pub struct NodeIdentifier<'model>(Arc<Node<'model>>);

impl<'model> Debug for NodeIdentifier<'model> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("NodeIdentifier")
            .field(&Arc::as_ptr(&self.0))
            .field(&self.0.definition.get_name())
            .finish()
    }
}

impl<'model> Hash for NodeIdentifier<'model> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        ptr::hash(Arc::as_ptr(&self.0), state)
    }
}

impl<'model> PartialEq for NodeIdentifier<'model> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<'model> Eq for NodeIdentifier<'model> {}

impl<'model> Node<'model> {
    pub fn identifier(self: &Arc<Self>) -> NodeIdentifier<'model> {
        NodeIdentifier(self.clone())
    }
}
